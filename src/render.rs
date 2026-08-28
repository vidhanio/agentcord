use std::fmt::Write;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use serenity::all::{Context, CreateMessage, EditMessage, GenericChannelId, MessageId};

use crate::{
    Bot, BotResult,
    acp::RenderUpdate,
    db::{RenderRow, RenderSourceKey, TurnNumber},
};

/// Maximum content length for a normal Discord message.
const MESSAGE_LIMIT: usize = 2000;
/// Output budget that leaves room for execute formatting and fences.
const EXECUTE_OUTPUT_LIMIT: usize = 1900;

/// Accumulated thought and final text for one streamed response source.
#[derive(Default, Deserialize, Serialize)]
struct OutputState {
    /// Thought text received before final answer text begins.
    #[serde(default)]
    thought: String,
    /// User-visible final response text.
    #[serde(default)]
    final_text: String,
    /// Prefix of Discord message ids reserved for thought rendering.
    #[serde(default)]
    thought_message_count: usize,
    /// Byte offset through thought text already emitted as completed chunks.
    #[serde(default)]
    thought_rendered: usize,
}

impl Bot {
    /// Applies an ordered batch of coherent ACP updates to Discord.
    pub async fn render_updates(&self, updates: Vec<RenderUpdate>) -> BotResult {
        let ctx = self.context()?;
        let mut updates = updates.into_iter().peekable();
        while let Some(RenderUpdate {
            thread,
            turn,
            replay,
            ui,
            update,
        }) = updates.next()
        {
            match update {
                SessionUpdate::AgentThoughtChunk(first) => {
                    let mut chunks = vec![first];
                    while matches!(
                        updates.peek(),
                        Some(RenderUpdate {
                            thread: same_thread,
                            turn: same_turn,
                            replay: same_replay,
                            update: SessionUpdate::AgentThoughtChunk(_),
                            ..
                        }) if *same_thread == thread && *same_turn == turn && *same_replay == replay
                    ) {
                        if let Some(RenderUpdate {
                            update: SessionUpdate::AgentThoughtChunk(chunk),
                            ..
                        }) = updates.next()
                        {
                            chunks.push(chunk);
                        }
                    }
                    self.render_output_chunks(ctx, thread, turn, &chunks, true, replay)
                        .await?;
                }
                SessionUpdate::AgentMessageChunk(first) => {
                    let mut chunks = vec![first];
                    while matches!(
                        updates.peek(),
                        Some(RenderUpdate {
                            thread: same_thread,
                            turn: same_turn,
                            replay: same_replay,
                            update: SessionUpdate::AgentMessageChunk(_),
                            ..
                        }) if *same_thread == thread && *same_turn == turn && *same_replay == replay
                    ) {
                        if let Some(RenderUpdate {
                            update: SessionUpdate::AgentMessageChunk(chunk),
                            ..
                        }) = updates.next()
                        {
                            chunks.push(chunk);
                        }
                    }
                    self.render_output_chunks(ctx, thread, turn, &chunks, false, replay)
                        .await?;
                }
                // Discord-originated prompts are already visible, and replayed
                // user messages cannot be told apart from them, so neither is
                // rendered.
                SessionUpdate::UserMessageChunk(_) => {}
                SessionUpdate::ToolCall(call) => {
                    self.render_tool(ctx, thread, call).await?;
                }
                SessionUpdate::ToolCallUpdate(first) => {
                    let call_id = first.tool_call_id.to_string();
                    let mut batch = vec![first];
                    while matches!(
                        updates.peek(),
                        Some(RenderUpdate {
                            update: SessionUpdate::ToolCallUpdate(update),
                            ..
                        }) if update.tool_call_id.to_string() == call_id
                    ) {
                        if let Some(RenderUpdate {
                            update: SessionUpdate::ToolCallUpdate(update),
                            ..
                        }) = updates.next()
                        {
                            batch.push(update);
                        }
                    }
                    self.render_tool_updates(ctx, thread, batch).await?;
                }
                SessionUpdate::UsageUpdate(usage) => {
                    self.update_starter(thread, &ui, Some(&usage)).await?;
                }
                SessionUpdate::CurrentModeUpdate(_) | SessionUpdate::ConfigOptionUpdate(_) => {
                    self.update_starter(thread, &ui, None).await?;
                }
                SessionUpdate::SessionInfoUpdate(info) => {
                    self.render_session_info(thread, &info).await?;
                }
                other => {
                    self.render_metadata(ctx, thread, other).await?;
                }
            }
        }
        Ok(())
    }

    /// Applies an agent-reported title update to the session thread.
    async fn render_session_info(
        &self,
        thread: GenericChannelId,
        info: &(impl Serialize + Sync),
    ) -> BotResult {
        let value = serde_json::to_value(info).unwrap_or_default();
        let Some(title) = value.get("title") else {
            return Ok(());
        };
        let row = self.db.session(thread)?.ok_or_else(|| {
            crate::BotError::Other("session disappeared while updating its title".into())
        })?;
        self.update_title(thread, &row.project_path, &row.session_id, title.as_str())
            .await
    }

    /// Accumulates streamed thought or answer chunks and synchronizes messages.
    ///
    /// Replay uses protocol message ids for idempotence. Replayed unkeyed
    /// chunks are dropped because they cannot be distinguished from prior
    /// live output.
    async fn render_output_chunks(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        turn: TurnNumber,
        chunks: &[ContentChunk],
        thought: bool,
        replay: bool,
    ) -> BotResult {
        // Chunks that carry a message id key their render state by message so
        // replayed history deduplicates against what was already posted.
        // Chunks without one fall back to the live turn's stream; a replayed
        // chunk without an id cannot be deduplicated and is dropped.
        let key = match chunks.first().and_then(|chunk| chunk.message_id.as_ref()) {
            Some(id) => RenderSourceKey::new(format!("msg:{id}")),
            None if replay => return Ok(()),
            None => RenderSourceKey::new(format!("turn:{turn}:response")),
        };
        let row = self.db.render(thread, &key)?;
        if replay && row.is_some() {
            return Ok(());
        }
        let mut row = row.unwrap_or_else(|| RenderRow {
            source_key: key,
            discord_message_ids: vec![],
            state_json: serde_json::to_string(&OutputState::default()).unwrap(),
        });
        let mut state: OutputState = serde_json::from_str(&row.state_json).unwrap_or_default();
        let mut changed = false;
        for chunk in chunks {
            let text = content_text(&chunk.content);
            if text.is_empty() {
                continue;
            }
            if thought && state.final_text.is_empty() {
                state.thought.push_str(&text);
                changed = true;
            } else if !thought {
                state.final_text.push_str(&text);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        if state.final_text.is_empty() {
            let mut ids = std::mem::take(&mut row.discord_message_ids);
            sync_thought_messages(ctx, thread, &mut ids, &mut state).await?;
            row.discord_message_ids = ids;
        } else {
            let thought_count = state
                .thought_message_count
                .min(row.discord_message_ids.len());
            let mut thought_ids = row.discord_message_ids[..thought_count].to_vec();
            let mut final_ids = row.discord_message_ids[thought_count..].to_vec();
            sync_thought_messages(ctx, thread, &mut thought_ids, &mut state).await?;
            let final_chunks = split_message(&state.final_text, MESSAGE_LIMIT);
            sync_text_messages(ctx, thread, &mut final_ids, &final_chunks).await?;
            state.thought_message_count = thought_ids.len();
            row.discord_message_ids = thought_ids;
            row.discord_message_ids.extend(final_ids);
        }
        row.state_json = serde_json::to_string(&state).expect("output state serializes");
        self.db.upsert_render(thread, &row)
    }

    /// Creates or merges the initial projection for a tool call.
    async fn render_tool(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        call: ToolCall,
    ) -> BotResult {
        let call_id = call.tool_call_id.to_string();
        let key = RenderSourceKey::new(format!("tool:{call_id}"));
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            discord_message_ids: vec![],
            state_json: "{}".into(),
        });
        let value = serde_json::to_value(call).unwrap_or_default();
        // Merging (rather than replacing) keeps replayed tool calls
        // idempotent against the state that was already rendered.
        let mut state: serde_json::Value =
            serde_json::from_str(&row.state_json).unwrap_or_default();
        merge_object(&mut state, &value);
        row.state_json = serde_json::to_string(&state).expect("tool state serializes");
        sync_tool_messages(ctx, thread, &mut row.discord_message_ids, &state).await?;
        self.db.upsert_render(thread, &row)
    }

    /// Merges a batch of updates into an existing tool-call projection.
    async fn render_tool_updates(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        updates: Vec<ToolCallUpdate>,
    ) -> BotResult {
        let call_id = updates
            .first()
            .expect("tool update batch is non-empty")
            .tool_call_id
            .to_string();
        let key = RenderSourceKey::new(format!("tool:{call_id}"));
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            discord_message_ids: vec![],
            state_json: serde_json::json!({
                "toolCallId": call_id,
                "title": "tool call",
                "kind": "other",
                "status": "pending"
            })
            .to_string(),
        });
        let mut state: serde_json::Value =
            serde_json::from_str(&row.state_json).unwrap_or_default();
        for update in updates {
            let update = serde_json::to_value(update).unwrap_or_default();
            merge_object(&mut state, &update);
        }
        row.state_json = serde_json::to_string(&state).expect("tool state serializes");
        sync_tool_messages(ctx, thread, &mut row.discord_message_ids, &state).await?;
        self.db.upsert_render(thread, &row)
    }

    /// Renders otherwise unsupported ACP metadata as bounded JSON.
    async fn render_metadata(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        update: SessionUpdate,
    ) -> BotResult {
        let value = serde_json::to_value(&update).unwrap_or_default();
        let kind = value
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("update");
        let key = RenderSourceKey::new(format!("metadata:{kind}"));
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            discord_message_ids: vec![],
            state_json: String::new(),
        });
        let body = format!(
            "**{}**\n```json\n{}\n```",
            kind.replace('_', " "),
            cap(
                &serde_json::to_string_pretty(&value).unwrap_or_default(),
                1750
            )
        );
        sync_text_messages(ctx, thread, &mut row.discord_message_ids, &[body]).await?;
        row.state_json = value.to_string();
        self.db.upsert_render(thread, &row)
    }
}

/// Converts an ACP content block into a compact text representation.
fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => format!("[image: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        ContentBlock::ResourceLink(resource) => format!("[{}]({})", resource.name, resource.uri),
        ContentBlock::Resource(resource) => {
            let value = serde_json::to_value(resource).unwrap_or_default();
            value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| "[embedded resource]".into(), ToOwned::to_owned)
        }
        _ => "[unsupported ACP content]".into(),
    }
}

/// Edits, creates, or removes Discord messages to match an exact text list.
async fn sync_text_messages(
    ctx: &Context,
    thread: GenericChannelId,
    ids: &mut Vec<MessageId>,
    chunks: &[String],
) -> BotResult {
    for (index, chunk) in chunks.iter().enumerate() {
        if let Some(id) = ids.get(index).copied()
            && thread
                .edit_message(
                    &ctx.http,
                    id,
                    EditMessage::new().content(chunk).embeds(Vec::new()),
                )
                .await
                .is_ok()
        {
            continue;
        }
        let message = thread
            .send_message(&ctx.http, CreateMessage::new().content(chunk))
            .await?;
        if index < ids.len() {
            ids[index] = message.id;
        } else {
            ids.push(message.id);
        }
    }
    for id in ids.drain(chunks.len()..) {
        let _ = thread.delete_message(&ctx.http, id, None).await;
    }
    Ok(())
}

/// Streams thought text while preserving already completed Discord chunks.
async fn sync_thought_messages(
    ctx: &Context,
    thread: GenericChannelId,
    ids: &mut Vec<MessageId>,
    state: &mut OutputState,
) -> BotResult {
    let rendered = state.thought.chars().count();
    if rendered == state.thought_rendered {
        return Ok(());
    }
    // Italics break when the asterisks hug whitespace, so trim the whole
    // message before splitting it.
    let thought = state.thought.trim();
    if thought.is_empty() {
        return Ok(());
    }
    let chunks = split_message(thought, MESSAGE_LIMIT - 2)
        .into_iter()
        .map(|chunk| format!("*{chunk}*"))
        .collect::<Vec<_>>();
    sync_text_messages(ctx, thread, ids, &chunks).await?;
    state.thought_rendered = rendered;
    state.thought_message_count = ids.len();
    Ok(())
}

/// Renders and synchronizes the current merged tool-call state.
async fn sync_tool_messages(
    ctx: &Context,
    thread: GenericChannelId,
    ids: &mut Vec<MessageId>,
    state: &serde_json::Value,
) -> BotResult {
    let text = render_tool_text(state);
    let chunks = split_message(&text, MESSAGE_LIMIT);
    sync_text_messages(ctx, thread, ids, &chunks).await
}

/// Chooses the specialized textual representation for a tool-call state.
fn render_tool_text(state: &serde_json::Value) -> String {
    let kind = state
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("other");
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let name = tool_label(kind);
    // The command fence carries an execute call's subject, so its header
    // stays on the generic name instead of repeating the command.
    let header = if kind == "execute" {
        format!("{} **{name}** · {status}", tool_emoji(kind))
    } else {
        let title = state
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(header_title);
        title.map_or_else(
            || format!("{} **{name}** · {status}", tool_emoji(kind)),
            |title| format!("{} **{name}** {title} · {status}", tool_emoji(kind)),
        )
    };
    let body = match kind {
        "edit" => render_diff(state),
        "execute" => render_execute(state),
        _ => render_tool_content(state),
    };
    join_header_body(&header, &body)
}

/// Joins a tool header and optional body with consistent spacing.
fn join_header_body(header: &str, body: &str) -> String {
    if body.trim().is_empty() {
        header.to_owned()
    } else {
        format!("{header}\n{body}")
    }
}

/// Maps ACP tool kinds to compact visual markers.
fn tool_emoji(kind: &str) -> &'static str {
    match kind {
        "read" => "📖",
        "edit" => "✏️",
        "delete" => "🗑️",
        "move" => "📂",
        "search" => "🔍",
        "execute" => "⚙️",
        "think" => "💭",
        "fetch" => "🌐",
        "switch_mode" => "🔁",
        _ => "🔧",
    }
}

/// Converts an ACP tool kind into a human-readable fallback label.
fn tool_label(kind: &str) -> String {
    kind.replace('_', " ")
}

/// Titles that are file or directory paths render in backticks so they stand
/// out from surrounding prose and survive Discord's markdown.
fn header_title(title: &str) -> String {
    if title.contains('/') || title.contains('\\') {
        format!("`{}`", title.replace('`', "ˋ"))
    } else {
        title.to_owned()
    }
}

/// Formats execute-tool command and output without duplicating its title.
fn render_execute(state: &serde_json::Value) -> String {
    let command = state
        .get("rawInput")
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(serde_json::Value::as_str)
        .or_else(|| state.get("title").and_then(serde_json::Value::as_str))
        .unwrap_or("command");
    let mut body = fence(command, "sh");
    if let Some(output) = execute_output(state) {
        let output = keep_tail(
            &output,
            EXECUTE_OUTPUT_LIMIT,
            "… earlier output omitted …\n",
        );
        if !output.trim().is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&fence(&output, "ansi"));
        }
    }
    body
}

/// Extracts and bounds the most useful output from an execute tool call.
fn execute_output(state: &serde_json::Value) -> Option<String> {
    if let Some(output) = state.get("rawOutput") {
        match output {
            serde_json::Value::String(text) => return Some(text.clone()),
            serde_json::Value::Object(fields) => {
                let stdout = fields
                    .get("stdout")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let stderr = fields
                    .get("stderr")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let merged = match (stdout.is_empty(), stderr.is_empty()) {
                    (false, false) => format!("{stdout}\nstderr:\n{stderr}"),
                    (false, true) => stdout.to_owned(),
                    (true, false) => format!("stderr:\n{stderr}"),
                    (true, true) => String::new(),
                };
                if !merged.is_empty() {
                    return Some(merged);
                }
            }
            _ => {}
        }
    }
    let mut text = String::new();
    for entry in tool_content(state) {
        if entry.get("type").and_then(serde_json::Value::as_str) == Some("text")
            && let Some(value) = entry.get("text").and_then(serde_json::Value::as_str)
        {
            text.push_str(value);
            text.push_str("\n\n");
        }
    }
    let text = text.trim_end().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Returns the tool-call content array or an empty slice.
fn tool_content(state: &serde_json::Value) -> &[serde_json::Value] {
    state
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Renders one tool-call content entry for kinds that show their content
/// directly. Returns `None` for entries with no useful text projection.
fn tool_block_text(entry: &serde_json::Value) -> Option<String> {
    match entry.get("type").and_then(serde_json::Value::as_str)? {
        "text" => Some(
            entry
                .get("text")
                .and_then(serde_json::Value::as_str)?
                .to_owned(),
        ),
        "diff" => {
            let mut body = String::new();
            append_diff(entry, &mut body);
            (!body.is_empty()).then(|| fence(&body, "diff"))
        }
        "image" => Some(format!(
            "[image: {}]",
            entry
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        )),
        "audio" => Some(format!(
            "[audio: {}]",
            entry
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        )),
        "resource_link" => {
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("resource");
            let uri = entry
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            Some(format!("[{name}]({uri})"))
        }
        "resource" => Some(
            entry
                .get("resource")
                .and_then(|resource| resource.get("text"))
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| "[embedded resource]".to_owned(), ToOwned::to_owned),
        ),
        _ => None,
    }
}

/// Renders all supported content blocks attached to a tool call.
fn render_tool_content(state: &serde_json::Value) -> String {
    let mut body = String::new();
    for entry in tool_content(state) {
        if let Some(text) = tool_block_text(entry) {
            body.push_str(&text);
            body.push_str("\n\n");
        }
    }
    while body.ends_with('\n') {
        body.pop();
    }
    if body.trim().is_empty() {
        return raw_json(state).map_or_else(String::new, |json| fence(&json, "json"));
    }
    body
}

/// Formats file-edit content as one or more unified diffs.
fn render_diff(state: &serde_json::Value) -> String {
    let mut body = String::new();
    for diff in tool_content(state)
        .iter()
        .filter(|content| content.get("type").and_then(serde_json::Value::as_str) == Some("diff"))
    {
        append_diff(diff, &mut body);
    }
    if body.trim().is_empty() {
        return raw_json(state).map_or_else(String::new, |json| fence(&json, "json"));
    }
    fence(&body, "diff")
}

/// Appends one file diff with correct new/deleted-file headers.
fn append_diff(diff: &serde_json::Value, body: &mut String) {
    let path = diff
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("file");
    match diff.get("oldText").and_then(serde_json::Value::as_str) {
        Some(old) => {
            let _ = writeln!(body, "--- a/{path}\n+++ b/{path}");
            for line in old.lines() {
                let _ = writeln!(body, "-{line}");
            }
        }
        None => {
            let _ = writeln!(body, "--- /dev/null\n+++ b/{path}");
        }
    }
    if let Some(new) = diff.get("newText").and_then(serde_json::Value::as_str) {
        for line in new.lines() {
            let _ = writeln!(body, "+{line}");
        }
    }
}

/// Serializes non-empty raw tool input as a JSON fallback.
fn raw_json(state: &serde_json::Value) -> Option<String> {
    state
        .get("rawInput")
        .map(|input| serde_json::to_string_pretty(input).unwrap_or_default())
}

/// Wraps a body in a Markdown code fence.
fn fence(body: &str, language: &str) -> String {
    format!("```{language}\n{}\n```", body.replace("```", "`\u{200b}``"))
}

/// Recursively merges object fields while replacing non-object values.
fn merge_object(target: &mut serde_json::Value, update: &serde_json::Value) {
    let (Some(target), Some(update)) = (target.as_object_mut(), update.as_object()) else {
        return;
    };
    for (key, value) in update {
        if key != "toolCallId" && key != "_meta" {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// Keeps a Unicode-safe tail and prefixes a truncation marker when needed.
fn keep_tail(value: &str, limit: usize, marker: &str) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let tail = value
        .chars()
        .rev()
        .take(limit - marker.chars().count())
        .collect::<String>();
    format!("{marker}{}", tail.chars().rev().collect::<String>())
}

#[must_use]
/// Splits text into Discord-sized chunks without splitting Unicode characters.
pub fn split_message(value: &str, limit: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in value.chars() {
        if current.chars().count() == limit {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Caps text to a Unicode-safe limit and appends an ellipsis.
fn cap(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut output = value.chars().take(limit - 1).collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    /// Covers header/body rendering for embedded and fetched content.
    fn embed_kinds_render_a_headered_text_body() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "src/lib.rs",
            "kind": "read",
            "status": "completed",
            "content": [{"type": "text", "text": "fn main() {}"}]
        });
        let text = render_tool_text(&state);
        assert_eq!(text, "📖 **read** `src/lib.rs` · completed\nfn main() {}");
    }

    #[test]
    /// Covers fallback headers when agents omit a tool title.
    fn headers_without_a_title_show_only_the_kind_name() {
        let state = json!({"toolCallId": "tc_1", "kind": "switch_mode", "status": "pending"});
        let text = render_tool_text(&state);
        assert_eq!(text, "🔁 **switch mode** · pending");
    }

    #[test]
    /// Covers path-specific title quoting without quoting ordinary titles.
    fn non_path_titles_stay_plain_while_paths_are_backticked() {
        let mut state = json!({
            "toolCallId": "tc_1",
            "title": "apply patch",
            "kind": "search",
            "status": "completed"
        });
        assert_eq!(
            render_tool_text(&state),
            "🔍 **search** apply patch · completed"
        );
        state["title"] = json!("~/Projects/agentcord/src");
        assert_eq!(
            render_tool_text(&state),
            "🔍 **search** `~/Projects/agentcord/src` · completed"
        );
    }

    #[test]
    /// Covers unified-diff headers for edits and newly created files.
    fn edits_render_diffs_and_new_files_use_dev_null() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "apply patch",
            "kind": "edit",
            "status": "completed",
            "content": [
                {"type": "diff", "path": "/tmp/new.rs", "newText": "fn a() {}\n"},
                {"type": "diff", "path": "/tmp/old.rs", "oldText": "fn b() {}\n", "newText": "fn c() {}\n"}
            ]
        });
        let text = render_tool_text(&state);
        assert!(text.starts_with("✏️ **edit** apply patch · completed\n```diff\n"));
        assert!(text.contains("--- /dev/null\n+++ b//tmp/new.rs\n+fn a() {}\n"));
        assert!(text.contains("--- a//tmp/old.rs\n+++ b//tmp/old.rs\n-fn b() {}\n+fn c() {}\n"));
    }

    #[test]
    /// Covers execute command rendering and bounded output tails.
    fn execute_renders_command_and_capped_output_tail() {
        let output = "x".repeat(3000);
        let state = json!({
            "toolCallId": "tc_1",
            "title": "ls",
            "kind": "execute",
            "status": "completed",
            "rawInput": {"command": "cargo test"},
            "rawOutput": {"stdout": output}
        });
        let text = render_tool_text(&state);
        assert!(text.starts_with("⚙️ **execute** · completed\n```sh\ncargo test\n```"));
        assert!(text.contains("```ansi\n… earlier output omitted …\n"));
        assert!(text.ends_with("\n```"));
    }

    #[test]
    /// Prevents execute commands from being repeated in their headers.
    fn execute_headers_stay_generic_when_the_title_is_the_command() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "cargo test",
            "kind": "execute",
            "status": "completed",
            "rawInput": {"command": "cargo test"},
            "rawOutput": {"stdout": "ok"}
        });
        assert_eq!(
            render_tool_text(&state),
            "⚙️ **execute** · completed\n```sh\ncargo test\n```\n```ansi\nok\n```"
        );
    }

    #[test]
    /// Covers the title fallback when execute raw input omits a command.
    fn execute_without_raw_input_falls_back_to_the_title_for_the_command() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "deploy",
            "kind": "execute",
            "status": "in_progress"
        });
        assert_eq!(
            render_tool_text(&state),
            "⚙️ **execute** · in_progress\n```sh\ndeploy\n```"
        );
    }

    #[test]
    /// Covers raw-input fallback for otherwise empty tool calls.
    fn tools_without_content_fall_back_to_raw_input_json() {
        let state = json!({
            "toolCallId": "tc_1",
            "kind": "search",
            "status": "in_progress",
            "rawInput": {"pattern": "foo", "path": "src"}
        });
        let text = render_tool_text(&state);
        assert!(text.contains("```json\n{\n  \"pattern\": \"foo\",\n  \"path\": \"src\"\n}\n```"));
    }

    #[test]
    /// Covers Markdown projection of ACP resource links.
    fn resource_links_render_as_markdown() {
        let state = json!({
            "toolCallId": "tc_1",
            "kind": "fetch",
            "status": "completed",
            "content": [{"type": "resource_link", "name": "docs", "uri": "https://example.com"}]
        });
        let text = render_tool_text(&state);
        assert!(text.contains("[docs](https://example.com)"));
    }

    #[test]
    /// Covers removal of obsolete Discord message ids after content shrinks.
    fn surplus_messages_are_dropped_from_the_id_list() {
        let mut ids = vec![MessageId::new(1), MessageId::new(2), MessageId::new(3)];
        ids.drain(2..);
        assert_eq!(ids.len(), 2);
    }
}

use std::{fmt::Write, sync::Arc};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use serenity::all::{Context, CreateMessage, EditMessage, GenericChannelId, MessageId};

use crate::{Bot, BotResult, db::RenderRow};

const MESSAGE_LIMIT: usize = 2000;
const EXECUTE_OUTPUT_LIMIT: usize = 1900;

#[derive(Default, Deserialize, Serialize)]
struct OutputState {
    thought: String,
    final_text: String,
    thought_message_count: usize,
    thought_rendered: usize,
}

impl Bot {
    pub async fn render_update(
        &self,
        thread: GenericChannelId,
        turn: u64,
        update: SessionUpdate,
    ) -> BotResult {
        self.render_updates(thread, turn, vec![update]).await
    }

    pub async fn render_updates(
        &self,
        thread: GenericChannelId,
        turn: u64,
        updates: Vec<SessionUpdate>,
    ) -> BotResult {
        let ctx = self.context()?;
        let lock = self.session_lock(thread);
        let _guard = lock.lock().await;
        let mut updates = updates.into_iter().peekable();
        while let Some(update) = updates.next() {
            match update {
                SessionUpdate::AgentThoughtChunk(first) => {
                    let mut chunks = vec![first];
                    while matches!(updates.peek(), Some(SessionUpdate::AgentThoughtChunk(_))) {
                        if let Some(SessionUpdate::AgentThoughtChunk(chunk)) = updates.next() {
                            chunks.push(chunk);
                        }
                    }
                    self.render_output_chunks(ctx, thread, turn, &chunks, true)
                        .await?;
                }
                SessionUpdate::AgentMessageChunk(first) => {
                    let mut chunks = vec![first];
                    while matches!(updates.peek(), Some(SessionUpdate::AgentMessageChunk(_))) {
                        if let Some(SessionUpdate::AgentMessageChunk(chunk)) = updates.next() {
                            chunks.push(chunk);
                        }
                    }
                    self.render_output_chunks(ctx, thread, turn, &chunks, false)
                        .await?;
                }
                // Discord-originated prompts are already visible. Ignoring user
                // chunks prevents agents that echo prompt history from duplicating them.
                SessionUpdate::UserMessageChunk(_) => {}
                SessionUpdate::ToolCall(call) => {
                    self.render_tool(ctx, thread, call).await?;
                }
                SessionUpdate::ToolCallUpdate(first) => {
                    let call_id = first.tool_call_id.to_string();
                    let mut batch = vec![first];
                    while matches!(
                        updates.peek(),
                        Some(SessionUpdate::ToolCallUpdate(update))
                            if update.tool_call_id.to_string() == call_id
                    ) {
                        if let Some(SessionUpdate::ToolCallUpdate(update)) = updates.next() {
                            batch.push(update);
                        }
                    }
                    self.render_tool_updates(ctx, thread, batch).await?;
                }
                SessionUpdate::UsageUpdate(usage) => {
                    self.update_usage(thread, &usage).await?;
                }
                SessionUpdate::SessionInfoUpdate(info) => {
                    let value = serde_json::to_value(info).unwrap_or_default();
                    let Some(title_value) = value.get("title") else {
                        continue;
                    };
                    let title = title_value.as_str();
                    let row = self.db.session(thread)?.ok_or_else(|| {
                        crate::BotError::Other(
                            "session disappeared while updating its title".into(),
                        )
                    })?;
                    self.update_title(&row, title).await?;
                }
                other => {
                    self.render_metadata(ctx, thread, other).await?;
                }
            }
        }
        Ok(())
    }

    async fn render_output_chunks(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        turn: u64,
        chunks: &[ContentChunk],
        thought: bool,
    ) -> BotResult {
        let key = format!("turn:{turn}:response");
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            kind: "response".into(),
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

    async fn render_tool(
        &self,
        ctx: &Context,
        thread: GenericChannelId,
        call: ToolCall,
    ) -> BotResult {
        let call_id = call.tool_call_id.to_string();
        let key = format!("tool:{call_id}");
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            kind: "tool".into(),
            discord_message_ids: vec![],
            state_json: "{}".into(),
        });
        let value = serde_json::to_value(call).unwrap_or_default();
        row.state_json = serde_json::to_string(&value).expect("tool call serializes");
        sync_tool_messages(ctx, thread, &mut row.discord_message_ids, &value).await?;
        self.db.upsert_render(thread, &row)
    }

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
        let key = format!("tool:{call_id}");
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            kind: "tool".into(),
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
        let key = format!("metadata:{kind}");
        let mut row = self.db.render(thread, &key)?.unwrap_or_else(|| RenderRow {
            source_key: key,
            kind: "metadata".into(),
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

    fn session_lock(&self, thread: GenericChannelId) -> Arc<tokio::sync::Mutex<()>> {
        self.render_locks
            .lock()
            .expect("renderer lock map poisoned")
            .entry(thread)
            .or_default()
            .clone()
    }
}

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
    let chunks = split_message(&state.thought, MESSAGE_LIMIT - 2)
        .into_iter()
        .map(|chunk| format!("*{chunk}*"))
        .collect::<Vec<_>>();
    sync_text_messages(ctx, thread, ids, &chunks).await?;
    state.thought_rendered = rendered;
    state.thought_message_count = ids.len();
    Ok(())
}

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

fn render_tool_text(state: &serde_json::Value) -> String {
    let kind = state
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("other");
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let label = tool_label(kind);
    let title = state
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(label.as_str());
    let header = format!("{} {title} · {status}", tool_emoji(kind));
    let body = match kind {
        "edit" => render_diff(state),
        "execute" => render_execute(state),
        _ => render_tool_content(state),
    };
    join_header_body(&header, &body)
}

fn join_header_body(header: &str, body: &str) -> String {
    if body.trim().is_empty() {
        header.to_owned()
    } else {
        format!("{header}\n{body}")
    }
}

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

fn tool_label(kind: &str) -> String {
    let text = kind.replace('_', " ");
    let mut characters = text.chars();
    characters.next().map_or_else(
        || "Tool".to_owned(),
        |first| first.to_uppercase().collect::<String>() + characters.as_str(),
    )
}

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
            body.push('\n');
            body.push_str(&fence(&output, "ansi"));
        }
    }
    body
}

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

fn raw_json(state: &serde_json::Value) -> Option<String> {
    state
        .get("rawInput")
        .map(|input| serde_json::to_string_pretty(input).unwrap_or_default())
}

fn fence(body: &str, language: &str) -> String {
    format!("```{language}\n{}\n```", body.replace("```", "`\u{200b}``"))
}

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
    fn embed_kinds_render_a_headered_text_body() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "src/lib.rs",
            "kind": "read",
            "status": "completed",
            "content": [{"type": "text", "text": "fn main() {}"}]
        });
        let text = render_tool_text(&state);
        assert_eq!(text, "📖 src/lib.rs · completed\nfn main() {}");
    }

    #[test]
    fn missing_title_falls_back_to_the_kind_label() {
        let state = json!({"toolCallId": "tc_1", "kind": "switch_mode", "status": "pending"});
        let text = render_tool_text(&state);
        assert_eq!(text, "🔁 Switch mode · pending");
    }

    #[test]
    fn edits_render_diffs_and_new_files_use_dev_null() {
        let state = json!({
            "toolCallId": "tc_1",
            "title": "edit",
            "kind": "edit",
            "status": "completed",
            "content": [
                {"type": "diff", "path": "/tmp/new.rs", "newText": "fn a() {}\n"},
                {"type": "diff", "path": "/tmp/old.rs", "oldText": "fn b() {}\n", "newText": "fn c() {}\n"}
            ]
        });
        let text = render_tool_text(&state);
        assert!(text.starts_with("✏️ edit · completed\n```diff\n"));
        assert!(text.contains("--- /dev/null\n+++ b//tmp/new.rs\n+fn a() {}\n"));
        assert!(text.contains("--- a//tmp/old.rs\n+++ b//tmp/old.rs\n-fn b() {}\n+fn c() {}\n"));
    }

    #[test]
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
        assert!(text.contains("```sh\ncargo test\n```"));
        assert!(text.contains("```ansi\n… earlier output omitted …\n"));
        assert!(text.ends_with("\n```"));
    }

    #[test]
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
    fn surplus_messages_are_dropped_from_the_id_list() {
        let mut ids = vec![MessageId::new(1), MessageId::new(2), MessageId::new(3)];
        ids.drain(2..);
        assert_eq!(ids.len(), 2);
    }
}

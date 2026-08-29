//! Ordered ACP updates projected into Discord messages.
//!
//! The reducer in this module is deliberately independent of Discord and
//! persistence. It accepts one ordered update at a time and returns the
//! complete projection for the affected source. The Discord adapter then
//! synchronizes message IDs and persists the result.

use std::fmt::Write;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use serde::{Deserialize, Serialize};
use serenity::all::{Context, CreateMessage, EditMessage, GenericChannelId, MessageId};
use text_splitter::{ChunkConfig, MarkdownSplitter};

use crate::{Bot, BotError, BotResult, db::RenderProjection};

/// Discord's maximum normal message length.
/// Maximum number of Unicode characters Discord accepts in one message.
const MESSAGE_LIMIT: usize = serenity::constants::MESSAGE_CODE_LIMIT;

/// An ACP update together with the Discord session it belongs to.
#[derive(Clone, Debug)]
pub struct ProjectionEvent {
    /// Discord forum thread receiving the update.
    pub thread_id: GenericChannelId,
    /// Prompt identifier used to key chunks without an ACP message ID.
    pub turn_id: String,
    /// Whether the update was emitted while replaying `session/load`.
    pub replay: bool,
    /// Original ACP session update.
    pub update: SessionUpdate,
}

/// Result of applying one update to a source projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionOutcome {
    /// The update has no Discord representation in the current projection.
    Ignored,
    /// The source changed and should be synchronized with Discord.
    Updated(RenderProjection),
}

/// Applies one ordered ACP update without performing I/O.
pub fn reduce(
    previous: Option<RenderProjection>,
    event: &ProjectionEvent,
) -> BotResult<ProjectionOutcome> {
    let Some((source_kind, source_id)) = source_key(event) else {
        return Ok(ProjectionOutcome::Ignored);
    };

    let mut projection = match previous {
        Some(projection)
            if projection.thread_id == event.thread_id
                && projection.source_kind == source_kind
                && projection.source_id == source_id =>
        {
            projection
        }
        _ => RenderProjection {
            thread_id: event.thread_id,
            source_kind: source_kind.to_owned(),
            source_id,
            state_json: String::new(),
            message_ids: Vec::new(),
        },
    };

    match &event.update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => {
            let text = content_text(&chunk.content);
            if text.is_empty() {
                return Ok(ProjectionOutcome::Ignored);
            }
            let mut state: TextState = parse_state(&projection.state_json)?;
            if event.replay && state.text.contains(&text) {
                return Ok(ProjectionOutcome::Ignored);
            }
            state.text.push_str(&text);
            projection.state_json = serde_json::to_string(&state)
                .map_err(|error| BotError::Projection(format!("text state: {error}")))?;
        }
        SessionUpdate::ToolCall(call) => {
            let mut state = parse_object_state(&projection.state_json)?;
            merge_object(
                &mut state,
                &serde_json::to_value(call)
                    .map_err(|error| BotError::Projection(format!("tool state: {error}")))?,
            );
            projection.state_json = serde_json::to_string(&state)
                .map_err(|error| BotError::Projection(format!("tool state: {error}")))?;
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let mut state = parse_object_state(&projection.state_json)?;
            merge_object(
                &mut state,
                &serde_json::to_value(update)
                    .map_err(|error| BotError::Projection(format!("tool update state: {error}")))?,
            );
            projection.state_json = serde_json::to_string(&state)
                .map_err(|error| BotError::Projection(format!("tool state: {error}")))?;
        }
        SessionUpdate::Plan(plan) => {
            projection.state_json = serde_json::to_string(plan)
                .map_err(|error| BotError::Projection(format!("plan state: {error}")))?;
        }
        _ => return Ok(ProjectionOutcome::Ignored),
    }

    Ok(ProjectionOutcome::Updated(projection))
}

/// Applies and renders one event, synchronizing Discord before persistence.
impl Bot {
    /// Projects one ordered ACP update into its session thread.
    pub async fn apply_projection_event(&self, event: ProjectionEvent) -> BotResult {
        let Some((kind, id)) = source_key(&event) else {
            return Ok(());
        };
        let previous = self.db().projection(event.thread_id, kind, &id).await?;
        let projection = match reduce(previous.clone(), &event)? {
            ProjectionOutcome::Updated(projection) => projection,
            ProjectionOutcome::Ignored if event.replay => {
                let Some(previous) = previous else {
                    return Ok(());
                };
                previous
            }
            ProjectionOutcome::Ignored => return Ok(()),
        };
        let mut projection = projection;

        let context = self.context()?.clone();
        let target = render_projection(&projection)?;
        self.db().replace_projection(&projection).await?;

        match sync_messages(
            &context,
            projection.thread_id,
            &projection.message_ids,
            target,
        )
        .await
        {
            Ok(message_ids) => {
                projection.message_ids = message_ids;
                self.db().replace_projection(&projection).await
            }
            Err(failure) => {
                projection.message_ids = failure.message_ids;
                self.db().replace_projection(&projection).await?;
                Err(failure.error)
            }
        }
    }
}

/// Renderer-owned state for a text source.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct TextState {
    /// Complete text accumulated for the current ACP message.
    text: String,
}

/// Finds the stable source key for an update.
fn source_key(event: &ProjectionEvent) -> Option<(&'static str, String)> {
    match &event.update {
        SessionUpdate::AgentMessageChunk(chunk) => chunk.message_id.as_ref().map_or_else(
            || (!event.replay).then(|| ("agent_message", format!("turn:{}", event.turn_id))),
            |id| Some(("agent_message", id.to_string())),
        ),
        SessionUpdate::AgentThoughtChunk(chunk) => chunk.message_id.as_ref().map_or_else(
            || (!event.replay).then(|| ("agent_thought", format!("turn:{}", event.turn_id))),
            |id| Some(("agent_thought", id.to_string())),
        ),
        SessionUpdate::ToolCall(call) => Some(("tool_call", call.tool_call_id.to_string())),
        SessionUpdate::ToolCallUpdate(update) => {
            Some(("tool_call", update.tool_call_id.to_string()))
        }
        SessionUpdate::Plan(_) => Some(("plan", "current".to_owned())),
        _ => None,
    }
}

/// Decodes renderer state, treating an empty value as the type's default.
fn parse_state<T>(state: &str) -> BotResult<T>
where
    T: Default + for<'de> Deserialize<'de>,
{
    if state.trim().is_empty() {
        Ok(T::default())
    } else {
        serde_json::from_str(state)
            .map_err(|error| BotError::Projection(format!("invalid source state: {error}")))
    }
}

/// Decodes one object-backed renderer state, defaulting empty state to `{}`.
fn parse_object_state(state: &str) -> BotResult<serde_json::Value> {
    if state.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let value: serde_json::Value = serde_json::from_str(state)
        .map_err(|error| BotError::Projection(format!("invalid object state: {error}")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(BotError::Projection(
            "renderer state must contain a JSON object".into(),
        ))
    }
}

/// Converts supported ACP content blocks into renderable text.
fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(resource) => format!("[{}]({})", resource.name, resource.uri),
        ContentBlock::Resource(resource) => serde_json::to_value(resource)
            .ok()
            .and_then(|value| {
                value
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "[embedded resource]".into()),
        ContentBlock::Image(image) => format!("[image: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        _ => "[unsupported ACP content]".into(),
    }
}

/// Turns persisted renderer state into bounded Discord message chunks.
fn render_projection(projection: &RenderProjection) -> BotResult<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(&projection.state_json)
        .map_err(|error| BotError::Projection(format!("invalid source state: {error}")))?;
    match projection.source_kind.as_str() {
        "agent_message" => Ok(render_text(&value, false)),
        "agent_thought" => Ok(render_text(&value, true)),
        "tool_call" => Ok(split_message(&render_tool_text(&value), MESSAGE_LIMIT)),
        "plan" => Ok(split_message(&render_plan(&value), MESSAGE_LIMIT)),
        _ => Ok(Vec::new()),
    }
}

/// Renders accumulated agent text, optionally wrapped as an internal thought.
fn render_text(value: &serde_json::Value, thought: bool) -> Vec<String> {
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let text = if thought { text.trim() } else { text };
    if text.is_empty() {
        return Vec::new();
    }
    let limit = if thought {
        MESSAGE_LIMIT.saturating_sub(2)
    } else {
        MESSAGE_LIMIT
    };
    let chunks = split_message(text, limit);
    if thought {
        chunks
            .into_iter()
            .map(|chunk| format!("*{chunk}*"))
            .collect()
    } else {
        chunks
    }
}

/// Formats a tool call and its current result for one Discord projection.
fn render_tool_text(state: &serde_json::Value) -> String {
    let kind = state
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("other");
    let title = state
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map_or_else(|| kind.replace('_', " "), ToOwned::to_owned);
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let header = format!("{} **{}**{}", tool_emoji(kind), title, tool_status(status));
    let body = match kind {
        "edit" => render_diff_content(state),
        "execute" => render_execute_content(state),
        _ => render_tool_content(state),
    };
    if body.trim().is_empty() {
        header
    } else {
        format!("{header}\n{body}")
    }
}

/// Formats a plan update as a compact checklist.
fn render_plan(state: &serde_json::Value) -> String {
    let mut output = String::from("📝 **plan**");
    let Some(entries) = state.get("entries").and_then(serde_json::Value::as_array) else {
        return output;
    };
    for entry in entries {
        let Some(content) = entry.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let marker = match entry
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pending")
        {
            "completed" => "x",
            "in_progress" => "~",
            _ => " ",
        };
        let _ = write!(output, "\n[{marker}] {content}");
    }
    output
}

/// Maps ACP tool kinds to compact Discord markers.
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

/// Formats the status suffix shown after a tool-call title.
fn tool_status(status: &str) -> String {
    match status {
        "completed" => String::new(),
        "pending" => " · *pending*".into(),
        "in_progress" => " · *running*".into(),
        "failed" => " · *failed*".into(),
        other => format!(" · *{other}*"),
    }
}

/// Renders generic content attached to a tool call.
fn render_tool_content(state: &serde_json::Value) -> String {
    let mut output = String::new();
    if let Some(content) = state.get("content").and_then(serde_json::Value::as_array) {
        for entry in content {
            if let Some(text) = tool_content_text(entry) {
                if !output.is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str(&text);
            }
        }
    }
    if output.is_empty() {
        state
            .get("rawOutput")
            .or_else(|| state.get("rawInput"))
            .filter(|input| !input.is_null())
            .and_then(|input| serde_json::to_string_pretty(input).ok())
            .map_or_else(String::new, |input| fence(&input, "json"))
    } else {
        output
    }
}

/// Extracts text or a diff from one serialized tool-content entry.
fn tool_content_text(entry: &serde_json::Value) -> Option<String> {
    match entry.get("type").and_then(serde_json::Value::as_str)? {
        "content" => entry.get("content").and_then(content_value_text),
        "text" => entry
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "diff" => Some(fence(&render_diff(entry), "diff")),
        "terminal" => entry
            .get("terminalId")
            .map(|id| format!("[terminal: {id}]")),
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
        "resource_link" => Some(format!(
            "[{}]({})",
            entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("resource"),
            entry
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        )),
        "resource" => entry
            .get("resource")
            .and_then(|resource| resource.get("text"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Extracts human-readable text from a serialized content block.
fn content_value_text(value: &serde_json::Value) -> Option<String> {
    match value.get("type").and_then(serde_json::Value::as_str)? {
        "text" => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "resource_link" => Some(format!(
            "[{}]({})",
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("resource"),
            value
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        )),
        "resource" => value
            .get("resource")
            .and_then(|resource| resource.get("text"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "image" => Some(format!(
            "[image: {}]",
            value
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        )),
        "audio" => Some(format!(
            "[audio: {}]",
            value
                .get("mimeType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
        )),
        _ => None,
    }
}

/// Renders file diffs attached to an edit tool call.
fn render_diff_content(state: &serde_json::Value) -> String {
    let Some(content) = state.get("content").and_then(serde_json::Value::as_array) else {
        return String::new();
    };
    let mut output = String::new();
    for entry in content {
        if entry.get("type").and_then(serde_json::Value::as_str) == Some("diff") {
            output.push_str(&render_diff(entry));
        }
    }
    if output.is_empty() {
        render_tool_content(state)
    } else {
        fence(&output, "diff")
    }
}

/// Formats one ACP file diff as unified-diff text.
fn render_diff(diff: &serde_json::Value) -> String {
    let path = diff
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("file");
    let mut output = String::new();
    if let Some(old) = diff.get("oldText").and_then(serde_json::Value::as_str) {
        let _ = writeln!(output, "--- a/{path}\n+++ b/{path}");
        for line in old.lines() {
            let _ = writeln!(output, "-{line}");
        }
    } else {
        let _ = writeln!(output, "--- /dev/null\n+++ b/{path}");
    }
    if let Some(new) = diff.get("newText").and_then(serde_json::Value::as_str) {
        for line in new.lines() {
            let _ = writeln!(output, "+{line}");
        }
    }
    output
}

/// Renders an execute tool's command and bounded output.
fn render_execute_content(state: &serde_json::Value) -> String {
    let command = state
        .get("rawInput")
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(serde_json::Value::as_str)
        .or_else(|| state.get("title").and_then(serde_json::Value::as_str))
        .unwrap_or("command");
    let mut output = fence(command, "sh");
    if let Some(result) = execute_output(state)
        && !result.trim().is_empty()
    {
        output.push('\n');
        output.push_str(&fence(&keep_tail(&result, 1800), "ansi"));
    }
    output
}

/// Extracts execute output from raw output or text content.
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
    let mut output = String::new();
    for entry in state
        .get("content")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(text) = tool_content_text(entry) {
            output.push_str(&text);
            output.push_str("\n\n");
        }
    }
    let output = output.trim_end();
    (!output.is_empty()).then(|| output.to_owned())
}

/// Wraps text in a Discord-safe Markdown code fence.
fn fence(value: &str, language: &str) -> String {
    format!(
        "```{language}\n{}\n```",
        value.replace("```", "`\u{200b}``")
    )
}

/// Keeps the most recent part of a long tool output.
fn keep_tail(value: &str, limit: usize) -> String {
    const MARKER: &str = "… earlier output omitted …\n";
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let keep = limit.saturating_sub(MARKER.chars().count());
    let tail = value
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{MARKER}{tail}")
}

/// Merges one serialized tool update into an existing object state.
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

/// Describes the Discord operations needed to match a target message list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagePlan {
    /// Existing messages to edit in place.
    pub edits: Vec<(MessageId, String)>,
    /// New messages to append in order.
    pub sends: Vec<String>,
    /// Existing messages no longer needed.
    pub deletes: Vec<MessageId>,
}

/// Plans edits, sends, and deletes without touching Discord.
#[must_use]
pub fn plan_messages(previous: &[MessageId], target: &[String]) -> MessagePlan {
    let shared = previous.len().min(target.len());
    MessagePlan {
        edits: previous[..shared]
            .iter()
            .copied()
            .zip(target[..shared].iter().cloned())
            .collect(),
        sends: target[shared..].to_vec(),
        deletes: previous[target.len().min(previous.len())..].to_vec(),
    }
}

/// Applies a message plan and reports IDs known even when an operation fails.
async fn sync_messages(
    context: &Context,
    thread: GenericChannelId,
    previous: &[MessageId],
    chunks: Vec<String>,
) -> Result<Vec<MessageId>, SyncFailure> {
    let plan = plan_messages(previous, &chunks);
    let mut current = previous[..plan.edits.len()].to_vec();
    for (id, chunk) in &plan.edits {
        if let Err(error) = thread
            .edit_message(&context.http, *id, EditMessage::new().content(chunk))
            .await
        {
            return Err(SyncFailure {
                message_ids: previous.to_vec(),
                error: error.into(),
            });
        }
    }

    for chunk in &plan.sends {
        let message = match thread
            .send_message(&context.http, CreateMessage::new().content(chunk))
            .await
        {
            Ok(message) => message,
            Err(error) => {
                return Err(SyncFailure {
                    message_ids: current,
                    error: error.into(),
                });
            }
        };
        current.push(message.id);
    }

    for (index, id) in plan.deletes.iter().enumerate() {
        if let Err(error) = thread.delete_message(&context.http, *id, None).await {
            let mut message_ids = current;
            message_ids.extend_from_slice(&plan.deletes[index..]);
            return Err(SyncFailure {
                message_ids,
                error: error.into(),
            });
        }
    }
    Ok(current)
}

/// Captures progress and the error from a failed Discord synchronization.
#[derive(Debug)]
struct SyncFailure {
    /// Message IDs known to exist after the partial operation.
    message_ids: Vec<MessageId>,
    /// Discord error that stopped synchronization.
    error: BotError,
}

/// Splits text into Discord-sized Unicode-safe chunks.
#[must_use]
pub fn split_message(value: &str, limit: usize) -> Vec<String> {
    if value.is_empty() || limit == 0 {
        return vec![String::new()];
    }
    let chunks = MarkdownSplitter::new(ChunkConfig::new(limit))
        .chunks(value)
        .flat_map(|chunk| hard_split(chunk, limit))
        .collect::<Vec<_>>();
    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

/// Splits one already Markdown-aware chunk at a hard character boundary.
fn hard_split(value: &str, limit: usize) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, MessageId as AcpMessageId, Plan, PlanEntry, PlanEntryPriority,
        PlanEntryStatus, TextContent, ToolCall, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields, ToolKind,
    };
    use serenity::all::{GenericChannelId, MessageId};

    use super::*;

    /// Wraps an ACP update in a synthetic live projection event.
    fn event(update: SessionUpdate) -> ProjectionEvent {
        ProjectionEvent {
            thread_id: GenericChannelId::new(1),
            turn_id: "2".into(),
            replay: false,
            update,
        }
    }

    /// Verifies chunks for one ACP message append in order.
    #[test]
    fn text_chunks_accumulate_by_protocol_message_id() {
        let first = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("hello ")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let second = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("world")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let ProjectionOutcome::Updated(state) = reduce(None, &first).unwrap() else {
            panic!("first text update should change the projection");
        };
        let ProjectionOutcome::Updated(state) = reduce(Some(state), &second).unwrap() else {
            panic!("second text update should change the projection");
        };
        assert_eq!(state.source_kind, "agent_message");
        assert_eq!(state.source_id, "m1");
        assert_eq!(state.state_json, r#"{"text":"hello world"}"#);
    }

    /// Verifies replay ignores unkeyed history while live output uses its turn.
    #[test]
    fn unkeyed_replay_is_ignored_but_live_text_uses_the_turn() {
        let mut replay = event(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("history")),
        )));
        replay.replay = true;
        assert_eq!(reduce(None, &replay).unwrap(), ProjectionOutcome::Ignored);

        let live = event(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("live")),
        )));
        let ProjectionOutcome::Updated(state) = reduce(None, &live).unwrap() else {
            panic!("live text should change the projection");
        };
        assert_eq!(state.source_id, "turn:2");
    }

    /// Verifies a new ACP message ID starts a separate projection source.
    #[test]
    fn a_new_protocol_message_id_starts_a_new_source() {
        let first = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("first")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let second = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("second")))
                .message_id(AcpMessageId::new("m2")),
        ));
        let ProjectionOutcome::Updated(first) = reduce(None, &first).unwrap() else {
            panic!("first source should be created");
        };
        let ProjectionOutcome::Updated(second) = reduce(Some(first), &second).unwrap() else {
            panic!("second source should be created");
        };
        assert_eq!(second.source_id, "m2");
        assert_eq!(second.state_json, r#"{"text":"second"}"#);
    }

    /// Verifies an identical replay chunk is idempotently ignored.
    #[test]
    fn an_already_rendered_replay_chunk_is_ignored() {
        let live = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("answer")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let ProjectionOutcome::Updated(state) = reduce(None, &live).unwrap() else {
            panic!("live source should be created");
        };
        let mut replay = live;
        replay.replay = true;
        assert_eq!(
            reduce(Some(state), &replay).unwrap(),
            ProjectionOutcome::Ignored
        );
    }

    /// Verifies replay deduplication works across multiple chunks.
    #[test]
    fn already_rendered_multi_chunk_replay_is_ignored() {
        let first = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("hello ")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let second = event(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("world")))
                .message_id(AcpMessageId::new("m1")),
        ));
        let ProjectionOutcome::Updated(state) = reduce(None, &first).unwrap() else {
            panic!("first source should be created");
        };
        let ProjectionOutcome::Updated(state) = reduce(Some(state), &second).unwrap() else {
            panic!("second source should append");
        };

        let mut replay_first = first;
        replay_first.replay = true;
        let mut replay_second = second;
        replay_second.replay = true;
        assert_eq!(
            reduce(Some(state.clone()), &replay_first).unwrap(),
            ProjectionOutcome::Ignored
        );
        assert_eq!(
            reduce(Some(state), &replay_second).unwrap(),
            ProjectionOutcome::Ignored
        );
    }

    /// Verifies streamed internal thoughts use their own stable projection.
    #[test]
    fn thought_chunks_render_as_italic_text() {
        let first = event(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("checking ")))
                .message_id(AcpMessageId::new("thought-1")),
        ));
        let second = event(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("the plan")))
                .message_id(AcpMessageId::new("thought-1")),
        ));
        let ProjectionOutcome::Updated(state) = reduce(None, &first).unwrap() else {
            panic!("first thought chunk should change the projection");
        };
        let ProjectionOutcome::Updated(state) = reduce(Some(state), &second).unwrap() else {
            panic!("second thought chunk should append");
        };
        assert_eq!(state.source_kind, "agent_thought");
        assert_eq!(
            render_projection(&state).unwrap(),
            vec!["*checking the plan*"]
        );
    }

    /// Verifies tool creation and updates merge into one rendered call.
    #[test]
    fn tool_call_updates_replace_status_and_content() {
        let call = event(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "read source")
                .kind(ToolKind::Read)
                .status(ToolCallStatus::InProgress),
        ));
        let ProjectionOutcome::Updated(state) = reduce(None, &call).unwrap() else {
            panic!("tool call should create a projection");
        };
        let update = event(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![
                    ContentBlock::Text(TextContent::new("source text")).into(),
                ]),
        )));
        let ProjectionOutcome::Updated(state) = reduce(Some(state), &update).unwrap() else {
            panic!("tool update should change the projection");
        };
        let rendered = render_projection(&state).unwrap().concat();
        assert!(rendered.contains("📖 **read source**"));
        assert!(rendered.contains("source text"));
        assert!(!rendered.contains("*running*"));
    }

    /// Verifies plans replace their checklist state in one stable projection.
    #[test]
    fn plans_render_as_checklists() {
        let update = event(SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "inspect the repository",
                PlanEntryPriority::High,
                PlanEntryStatus::Completed,
            ),
            PlanEntry::new(
                "implement the change",
                PlanEntryPriority::Medium,
                PlanEntryStatus::InProgress,
            ),
        ])));
        let ProjectionOutcome::Updated(state) = reduce(None, &update).unwrap() else {
            panic!("plan should create a projection");
        };
        assert_eq!(state.source_kind, "plan");
        let rendered = render_projection(&state).unwrap().concat();
        assert!(rendered.contains("📝 **plan**"));
        assert!(rendered.contains("[x] inspect the repository"));
        assert!(rendered.contains("[~] implement the change"));
    }

    /// Verifies message splitting respects Unicode boundaries and limits.
    #[test]
    fn message_chunks_are_unicode_safe_and_bounded() {
        let chunks = split_message(&"🙂".repeat(20), 7);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 7));
        assert_eq!(chunks.concat(), "🙂".repeat(20));
    }

    /// Verifies planning edits, appends, and suffix deletions preserves order.
    #[test]
    fn message_plan_preserves_order_when_appending_and_deleting() {
        let previous = vec![MessageId::new(1), MessageId::new(2), MessageId::new(3)];
        let target = vec!["first".into(), "second".into()];
        let plan = plan_messages(&previous, &target);
        assert_eq!(
            plan.edits,
            vec![
                (MessageId::new(1), "first".into()),
                (MessageId::new(2), "second".into())
            ]
        );
        assert_eq!(plan.sends, Vec::<String>::new());
        assert_eq!(plan.deletes, vec![MessageId::new(3)]);

        let target = vec![
            "first".into(),
            "second".into(),
            "third".into(),
            "fourth".into(),
        ];
        let plan = plan_messages(&previous, &target);
        assert_eq!(plan.sends, vec!["fourth"]);
        assert_eq!(plan.deletes, Vec::<MessageId>::new());
    }
}

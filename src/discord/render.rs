//! Ordered ACP updates projected into Discord messages.
//!
//! The reducer in this module is deliberately independent of Discord and
//! persistence. It accepts one ordered update at a time and returns the
//! complete projection for the affected source. The Discord adapter then
//! synchronizes message IDs and persists the result.

use std::collections::VecDeque;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use serde::{Deserialize, Serialize};
use serenity::all::{Context, CreateMessage, EditMessage, GenericChannelId, MessageId};
use text_splitter::{ChunkConfig, MarkdownSplitter, TextSplitter};
use tracing::{debug, warn};

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
        SessionUpdate::UserMessageChunk(chunk)
        | SessionUpdate::AgentMessageChunk(chunk)
        | SessionUpdate::AgentThoughtChunk(chunk) => {
            let text = content_text(&chunk.content);
            if text.is_empty() {
                return Ok(ProjectionOutcome::Ignored);
            }
            let mut state: TextState = parse_state(&projection.state_json)?;
            if event.replay && state.text.contains(&text) {
                return Ok(ProjectionOutcome::Ignored);
            }
            state.text.push_str(&text);
            projection.state_json =
                serde_json::to_string(&state).map_err(|source| BotError::ProjectionSerialize {
                    context: "text state",
                    source,
                })?;
        }
        SessionUpdate::ToolCall(call) => {
            let mut state = parse_object_state(&projection.state_json)?;
            merge_object(
                &mut state,
                &serde_json::to_value(call).map_err(|source| BotError::ProjectionSerialize {
                    context: "tool state",
                    source,
                })?,
            );
            projection.state_json =
                serde_json::to_string(&state).map_err(|source| BotError::ProjectionSerialize {
                    context: "tool state",
                    source,
                })?;
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let mut state = parse_object_state(&projection.state_json)?;
            merge_object(
                &mut state,
                &serde_json::to_value(update).map_err(|source| BotError::ProjectionSerialize {
                    context: "tool update state",
                    source,
                })?,
            );
            projection.state_json =
                serde_json::to_string(&state).map_err(|source| BotError::ProjectionSerialize {
                    context: "tool state",
                    source,
                })?;
        }
        SessionUpdate::Plan(plan) => {
            projection.state_json =
                serde_json::to_string(plan).map_err(|source| BotError::ProjectionSerialize {
                    context: "plan state",
                    source,
                })?;
        }
        _ => return Ok(ProjectionOutcome::Ignored),
    }

    Ok(ProjectionOutcome::Updated(projection))
}

/// Applies and renders one event, persisting the resulting Discord projection.
impl Bot {
    /// Reduces updates into the projections that need Discord synchronization.
    async fn collect_projections(
        &self,
        events: Vec<ProjectionEvent>,
    ) -> BotResult<Vec<RenderProjection>> {
        let mut projections = Vec::new();
        for event in events {
            let Some((kind, id)) = source_key(&event) else {
                debug!(
                    thread = ?event.thread_id,
                    replay = event.replay,
                    "ignoring acp update without a discord projection"
                );
                continue;
            };
            let index = projections
                .iter()
                .position(|projection: &RenderProjection| {
                    projection.thread_id == event.thread_id
                        && projection.source_kind == kind
                        && projection.source_id == id
                });
            let previous = if let Some(index) = index {
                debug!(
                    thread = ?event.thread_id,
                    source_kind = kind,
                    source_id = %id,
                    "reusing in-memory discord projection"
                );
                Some(projections[index].clone())
            } else {
                debug!(
                    thread = ?event.thread_id,
                    source_kind = kind,
                    source_id = %id,
                    "loading persisted discord projection..."
                );
                let previous = self.db().projection(event.thread_id, kind, &id).await?;
                debug!(
                    thread = ?event.thread_id,
                    source_kind = kind,
                    source_id = %id,
                    found = previous.is_some(),
                    "loaded persisted discord projection"
                );
                previous
            };
            let projection = match reduce(previous.clone(), &event)? {
                ProjectionOutcome::Updated(projection) => {
                    debug!(
                        thread = ?event.thread_id,
                        source_kind = kind,
                        source_id = %id,
                        replay = event.replay,
                        "updated discord projection"
                    );
                    projection
                }
                ProjectionOutcome::Ignored if event.replay => {
                    let Some(previous) = previous else {
                        debug!(
                            thread = ?event.thread_id,
                            source_kind = kind,
                            source_id = %id,
                            "ignoring replayed acp update without persisted projection"
                        );
                        continue;
                    };
                    debug!(
                        thread = ?event.thread_id,
                        source_kind = kind,
                        source_id = %id,
                        "retaining persisted projection for replayed acp update"
                    );
                    previous
                }
                ProjectionOutcome::Ignored => {
                    debug!(
                        thread = ?event.thread_id,
                        source_kind = kind,
                        source_id = %id,
                        "ignoring acp update without a projection change"
                    );
                    continue;
                }
            };
            if let Some(index) = index {
                projections[index] = projection;
            } else {
                projections.push(projection);
            }
        }
        Ok(projections)
    }

    /// Projects one ordered ACP update into its session thread.
    pub async fn apply_projection_event(&self, event: ProjectionEvent) -> BotResult {
        self.apply_projection_events(vec![event]).await
    }

    /// Projects adjacent ordered ACP updates with one Discord reconciliation
    /// per source.
    pub(crate) async fn apply_projection_events(&self, events: Vec<ProjectionEvent>) -> BotResult {
        debug!(
            events = events.len(),
            "processing acp updates for discord..."
        );
        let projections = self.collect_projections(events).await?;
        if projections.is_empty() {
            debug!("no discord projection changes to synchronize");
            return Ok(());
        }

        let context = self.context()?.clone();
        let targets = render_projections(projections)?;
        for (projection, target) in targets {
            self.synchronize_projection(&context, projection, target)
                .await?;
        }
        Ok(())
    }

    /// Synchronizes one rendered source and persists its Discord message IDs.
    async fn synchronize_projection(
        &self,
        context: &Context,
        mut projection: RenderProjection,
        target: Vec<String>,
    ) -> BotResult {
        debug!(
            thread = ?projection.thread_id,
            source_kind = %projection.source_kind,
            source_id = %projection.source_id,
            previous_messages = projection.message_ids.len(),
            target_messages = target.len(),
            "synchronizing rendered discord messages..."
        );
        let result = if projection.source_kind == "user_message" {
            self.sync_user_message_projection(
                context,
                projection.thread_id,
                &projection.message_ids,
                target,
            )
            .await
        } else {
            sync_messages(
                context,
                projection.thread_id,
                &projection.message_ids,
                target,
            )
            .await
        };
        match result {
            Ok(message_ids) => {
                debug!(
                    thread = ?projection.thread_id,
                    source_kind = %projection.source_kind,
                    source_id = %projection.source_id,
                    messages = message_ids.len(),
                    "synchronized rendered discord messages"
                );
                projection.message_ids = message_ids;
                debug!(
                    thread = ?projection.thread_id,
                    source_kind = %projection.source_kind,
                    source_id = %projection.source_id,
                    "storing synchronized discord projection..."
                );
                self.db().replace_projection(&projection).await?;
                debug!(
                    thread = ?projection.thread_id,
                    source_kind = %projection.source_kind,
                    source_id = %projection.source_id,
                    "stored synchronized discord projection"
                );
            }
            Err(SyncFailure { message_ids, error }) => {
                warn!(
                    ?error,
                    thread = ?projection.thread_id,
                    source_kind = %projection.source_kind,
                    source_id = %projection.source_id,
                    messages = message_ids.len(),
                    "failed to synchronize rendered discord messages"
                );
                projection.message_ids = message_ids;
                self.db().replace_projection(&projection).await?;
                return Err(*error);
            }
        }
        Ok(())
    }
}

/// Renders each changed projection into Discord-sized message chunks.
fn render_projections(
    projections: Vec<RenderProjection>,
) -> BotResult<Vec<(RenderProjection, Vec<String>)>> {
    projections
        .into_iter()
        .map(|projection| {
            debug!(
                thread = ?projection.thread_id,
                source_kind = %projection.source_kind,
                source_id = %projection.source_id,
                "rendering discord projection..."
            );
            let target = render_projection(&projection)?;
            debug!(
                thread = ?projection.thread_id,
                source_kind = %projection.source_kind,
                source_id = %projection.source_id,
                messages = target.len(),
                "rendered discord projection"
            );
            Ok((projection, target))
        })
        .collect()
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
        SessionUpdate::UserMessageChunk(chunk) if event.replay => chunk
            .message_id
            .as_ref()
            .map(|id| ("user_message", id.to_string())),
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
        serde_json::from_str(state).map_err(|source| BotError::ProjectionDeserialize {
            context: "source state",
            source,
        })
    }
}

/// Decodes one object-backed renderer state, defaulting empty state to `{}`.
fn parse_object_state(state: &str) -> BotResult<serde_json::Value> {
    if state.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let value: serde_json::Value =
        serde_json::from_str(state).map_err(|source| BotError::ProjectionDeserialize {
            context: "object state",
            source,
        })?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(BotError::ProjectionStateNotObject)
    }
}

/// Converts supported ACP content blocks into renderable text.
fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(resource) => format!("[{}]({})", resource.name, resource.uri),
        ContentBlock::Resource(resource) => match serde_json::to_value(resource) {
            Ok(value) => value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| "[embedded resource]".into(), str::to_owned),
            Err(error) => {
                warn!(?error, "failed to serialize acp resource content");
                "[embedded resource]".into()
            }
        },
        ContentBlock::Image(image) => format!("[image: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        _ => "[unsupported acp content]".into(),
    }
}

/// Turns persisted renderer state into bounded Discord message chunks.
fn render_projection(projection: &RenderProjection) -> BotResult<Vec<String>> {
    let value: serde_json::Value =
        serde_json::from_str(&projection.state_json).map_err(|source| {
            BotError::ProjectionDeserialize {
                context: "source state",
                source,
            }
        })?;
    match projection.source_kind.as_str() {
        "user_message" | "agent_message" => Ok(render_text(&value, false)),
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
    let name = tool_label(kind);
    let title = state
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .filter(|title| !title.replace('_', " ").eq_ignore_ascii_case(&name))
        .filter(|title| !(kind == "execute" && redundant_execute_title(title, state)));
    if kind == "read" {
        return title.map_or_else(
            || format!("{} **{name}**", tool_emoji(kind)),
            |title| {
                format!(
                    "{} **{name}** {}",
                    tool_emoji(kind),
                    format_tool_title(title)
                )
            },
        );
    }
    let title = title.map(header_title);
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let header = title.map_or_else(
        || format!("{} **{name}**{}", tool_emoji(kind), tool_status(status)),
        |title| {
            format!(
                "{} **{name}** {title}{}",
                tool_emoji(kind),
                tool_status(status)
            )
        },
    );
    let body = match kind {
        "edit" => render_diff_content(state),
        "execute" => render_execute_content(state),
        "search" => String::new(),
        _ => render_tool_content(state),
    };
    if body.trim().is_empty() {
        header
    } else {
        format!("{header}\n{body}")
    }
}

/// Converts a protocol tool kind into the name shown in a tool header.
fn tool_label(kind: &str) -> String {
    kind.replace('_', " ")
}

/// Wraps tool titles in inline Markdown code.
fn header_title(title: &str) -> String {
    format_tool_title(title)
}

/// Formats a tool title as escaped inline Markdown code.
fn format_tool_title(title: &str) -> String {
    format!("`{}`", title.replace('`', "ˋ"))
}

/// Recognizes execute titles that repeat the command, with or without a shell
/// prompt prefix.
fn redundant_execute_title(title: &str, state: &serde_json::Value) -> bool {
    let command = execute_command(state).trim();
    title == command || title == format!("$ {command}")
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
        output.push('\n');
        output.push('[');
        output.push_str(marker);
        output.push_str("] ");
        output.push_str(content);
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
        let Some(input) = state
            .get("rawOutput")
            .or_else(|| state.get("rawInput"))
            .filter(|input| !input.is_null())
        else {
            return String::new();
        };
        match serde_json::to_string_pretty(input) {
            Ok(input) => fence(&input, "json"),
            Err(error) => {
                warn!(?error, "failed to serialize acp tool content");
                String::new()
            }
        }
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
        output.push_str("--- a/");
        output.push_str(path);
        output.push_str("\n+++ b/");
        output.push_str(path);
        output.push('\n');
        for line in old.lines() {
            output.push('-');
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str("--- /dev/null\n+++ b/");
        output.push_str(path);
        output.push('\n');
    }
    if let Some(new) = diff.get("newText").and_then(serde_json::Value::as_str) {
        for line in new.lines() {
            output.push('+');
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

/// Renders only the command run by an execute tool.
fn render_execute_content(state: &serde_json::Value) -> String {
    fence(execute_command(state), "sh")
}

/// Resolves the command represented by an execute tool call.
fn execute_command(state: &serde_json::Value) -> &str {
    state
        .get("rawInput")
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(serde_json::Value::as_str)
        .or_else(|| state.get("title").and_then(serde_json::Value::as_str))
        .unwrap_or("command")
}

/// Wraps text in a Discord-safe Markdown code fence.
fn fence(value: &str, language: &str) -> String {
    format!(
        "```{language}\n{}\n```",
        value.replace("```", "`\u{200b}``")
    )
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
pub(crate) async fn sync_messages(
    context: &Context,
    thread: GenericChannelId,
    previous: &[MessageId],
    chunks: Vec<String>,
) -> Result<Vec<MessageId>, SyncFailure> {
    let plan = plan_messages(previous, &chunks);
    debug!(
        thread = ?thread,
        previous = previous.len(),
        target = chunks.len(),
        edits = plan.edits.len(),
        sends = plan.sends.len(),
        deletes = plan.deletes.len(),
        "planning rendered discord message synchronization..."
    );
    let current = edit_messages(context, thread, previous, &plan.edits).await?;
    let current = send_messages(context, thread, current, &plan.sends).await?;
    let current = delete_messages(context, thread, current, &plan.deletes).await?;
    debug!(
        thread = ?thread,
        messages = current.len(),
        "synchronized rendered discord message plan"
    );
    Ok(current)
}

/// Edits the existing messages in a synchronization plan.
async fn edit_messages(
    context: &Context,
    thread: GenericChannelId,
    previous: &[MessageId],
    edits: &[(MessageId, String)],
) -> Result<Vec<MessageId>, SyncFailure> {
    let current = previous[..edits.len()].to_vec();
    for (id, chunk) in edits {
        debug!(
            thread = ?thread,
            message = ?id,
            characters = chunk.chars().count(),
            "editing rendered discord message..."
        );
        match thread
            .edit_message(&context.http, *id, EditMessage::new().content(chunk))
            .await
        {
            Ok(_) => debug!(thread = ?thread, message = ?id, "edited rendered discord message"),
            Err(error) => {
                warn!(
                    ?error,
                    thread = ?thread,
                    message = ?id,
                    "failed to edit rendered discord message"
                );
                return Err(SyncFailure {
                    message_ids: previous.to_vec(),
                    error: Box::new(error.into()),
                });
            }
        }
    }
    Ok(current)
}

/// Sends new messages in a synchronization plan.
pub(crate) async fn send_messages(
    context: &Context,
    thread: GenericChannelId,
    mut current: Vec<MessageId>,
    sends: &[String],
) -> Result<Vec<MessageId>, SyncFailure> {
    for (index, chunk) in sends.iter().enumerate() {
        debug!(
            thread = ?thread,
            chunk = index + 1,
            chunks = sends.len(),
            characters = chunk.chars().count(),
            "sending rendered discord message..."
        );
        let message = match thread
            .send_message(&context.http, CreateMessage::new().content(chunk))
            .await
        {
            Ok(message) => {
                debug!(
                    thread = ?thread,
                    message = ?message.id,
                    chunk = index + 1,
                    "sent rendered discord message"
                );
                message
            }
            Err(error) => {
                warn!(
                    ?error,
                    thread = ?thread,
                    chunk = index + 1,
                    "failed to send rendered discord message"
                );
                return Err(SyncFailure {
                    message_ids: current,
                    error: Box::new(error.into()),
                });
            }
        };
        current.push(message.id);
    }
    Ok(current)
}

/// Deletes stale messages in a synchronization plan.
async fn delete_messages(
    context: &Context,
    thread: GenericChannelId,
    current: Vec<MessageId>,
    deletes: &[MessageId],
) -> Result<Vec<MessageId>, SyncFailure> {
    for (index, id) in deletes.iter().enumerate() {
        debug!(
            thread = ?thread,
            message = ?id,
            "deleting stale rendered discord message..."
        );
        match thread.delete_message(&context.http, *id, None).await {
            Ok(()) => {
                debug!(thread = ?thread, message = ?id, "deleted stale rendered discord message");
            }
            Err(error) => {
                warn!(
                    ?error,
                    thread = ?thread,
                    message = ?id,
                    "failed to delete stale rendered discord message"
                );
                let mut message_ids = current.clone();
                message_ids.extend_from_slice(&deletes[index..]);
                return Err(SyncFailure {
                    message_ids,
                    error: Box::new(error.into()),
                });
            }
        }
    }
    Ok(current)
}

/// Captures progress and the error from a failed Discord synchronization.
#[derive(Debug)]
pub(crate) struct SyncFailure {
    /// Message IDs known to exist after the partial operation.
    pub(crate) message_ids: Vec<MessageId>,
    /// Discord error that stopped synchronization.
    pub(crate) error: Box<BotError>,
}

/// Splits text into Discord-sized Unicode-safe chunks while preserving fenced
/// code blocks in every message that contains part of one.
#[must_use]
pub fn split_message(value: &str, limit: usize) -> Vec<String> {
    if value.is_empty() || limit == 0 {
        return vec![String::new()];
    }

    let mut messages = Vec::new();
    let mut current = String::new();
    for part in message_parts(value) {
        match part {
            MessagePart::Text(text) => {
                append_text_parts(text, &mut current, &mut messages, limit);
            }
            MessagePart::Code {
                opening,
                body,
                closing,
            } => append_code_parts(opening, body, closing, &mut current, &mut messages, limit),
        }
    }
    if !current.is_empty() {
        messages.push(current);
    }
    if messages.is_empty() {
        vec![String::new()]
    } else {
        messages
    }
}

/// One top-level region of a message, with fenced code kept separate from
/// surrounding Markdown.
enum MessagePart<'a> {
    /// Ordinary Markdown or text.
    Text(&'a str),
    /// A complete fenced code block and its original delimiters.
    Code {
        /// Opening fence, including its language and line ending.
        opening: &'a str,
        /// Code between the delimiters.
        body: &'a str,
        /// Closing fence.
        closing: &'a str,
    },
}

/// Finds complete fenced code blocks without changing their original text.
fn message_parts(value: &str) -> Vec<MessagePart<'_>> {
    let mut offset = 0;
    let lines = value
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            (start, offset, line)
        })
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    let mut text_start = 0;
    let mut line_index = 0;
    while line_index < lines.len() {
        let (opening_start, opening_end, opening_line) = lines[line_index];
        let Some((marker, marker_length, _)) = fence_marker(opening_line) else {
            line_index += 1;
            continue;
        };
        let closing = lines[line_index + 1..].iter().enumerate().find_map(
            |(relative_index, &(closing_start, closing_end, closing_line))| {
                let (closing_marker, closing_length, is_closing) = fence_marker(closing_line)?;
                (is_closing && closing_marker == marker && closing_length >= marker_length)
                    .then_some((line_index + relative_index + 1, closing_start, closing_end))
            },
        );
        let Some((closing_index, closing_start, closing_end)) = closing else {
            line_index += 1;
            continue;
        };

        if text_start < opening_start {
            parts.push(MessagePart::Text(&value[text_start..opening_start]));
        }
        parts.push(MessagePart::Code {
            opening: &value[opening_start..opening_end],
            body: &value[opening_end..closing_start],
            closing: &value[closing_start..closing_end],
        });
        text_start = closing_end;
        line_index = closing_index + 1;
    }
    if text_start < value.len() {
        parts.push(MessagePart::Text(&value[text_start..]));
    }
    parts
}

/// Returns a Markdown fence marker, including whether it can close a block.
fn fence_marker(line: &str) -> Option<(char, usize, bool)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let line = line.trim_start_matches([' ', '\t']);
    let marker = line.chars().next()?;
    if marker != '\x60' && marker != '~' {
        return None;
    }
    let mut marker_end = 0;
    let mut marker_length = 0;
    for (index, character) in line.char_indices() {
        if character != marker {
            break;
        }
        marker_end = index + character.len_utf8();
        marker_length += 1;
    }
    (marker_length >= 3).then(|| (marker, marker_length, line[marker_end..].trim().is_empty()))
}

/// Appends Markdown chunks to the current message, preserving logical splits.
fn append_text_parts(value: &str, current: &mut String, messages: &mut Vec<String>, limit: usize) {
    for chunk in markdown_chunks(value, limit) {
        append_chunk(&chunk, current, messages, limit);
    }
}

/// Appends one fenced block, splitting only its body and repeating its fences.
fn append_code_parts(
    opening: &str,
    body: &str,
    closing: &str,
    current: &mut String,
    messages: &mut Vec<String>,
    limit: usize,
) {
    let mut chunks = code_body_chunks(body, opening, closing, limit);
    let Some(first) = chunks.first().cloned() else {
        return;
    };
    chunks.remove(0);

    if !current.is_empty() {
        let available = limit.saturating_sub(current.chars().count());
        let fitting = code_body_chunks(&first, opening, closing, available);
        if let Some(fitting_first) = fitting.first() {
            let rendered = fenced_chunk(opening, fitting_first, closing);
            if fits(current, &rendered, limit) {
                current.push_str(&rendered);
                let mut remaining = fitting.into_iter().skip(1).collect::<String>();
                for chunk in chunks {
                    remaining.push_str(&chunk);
                }
                if !remaining.is_empty() {
                    append_code_remainder(
                        code_body_chunks(&remaining, opening, closing, limit),
                        opening,
                        closing,
                        current,
                        messages,
                    );
                }
                return;
            }
        }
        messages.push(std::mem::take(current));
    }

    current.push_str(&fenced_chunk(opening, &first, closing));
    append_code_remainder(chunks, opening, closing, current, messages);
}

/// Appends remaining fenced chunks, leaving the last one open for later text.
fn append_code_remainder(
    chunks: impl IntoIterator<Item = String>,
    opening: &str,
    closing: &str,
    current: &mut String,
    messages: &mut Vec<String>,
) {
    for body in chunks {
        messages.push(std::mem::take(current));
        current.push_str(&fenced_chunk(opening, &body, closing));
    }
}

/// Appends one already bounded chunk or starts the next message.
fn append_chunk(chunk: &str, current: &mut String, messages: &mut Vec<String>, limit: usize) {
    if chunk.is_empty() {
        return;
    }
    if !fits(current, chunk, limit) && !current.is_empty() {
        messages.push(std::mem::take(current));
    }
    current.push_str(chunk);
}

/// Splits ordinary Markdown with the Markdown-aware text splitter path.
fn markdown_chunks(value: &str, limit: usize) -> Vec<String> {
    MarkdownSplitter::new(ChunkConfig::new(limit).with_trim(false))
        .chunks(value)
        .map(str::to_owned)
        .collect()
}

/// Splits code contents with line-aware text splitter semantics.
fn text_chunks(value: &str, limit: usize) -> Vec<String> {
    TextSplitter::new(ChunkConfig::new(limit).with_trim(false))
        .chunks(value)
        .map(str::to_owned)
        .collect()
}

/// Splits a code body and ensures every reconstructed fence fits its limit.
fn code_body_chunks(body: &str, opening: &str, closing: &str, limit: usize) -> Vec<String> {
    if body.is_empty() {
        return vec![String::new()];
    }
    let overhead = opening.chars().count() + closing.chars().count();
    let body_limit = limit.saturating_sub(overhead);
    if body_limit == 0 {
        return vec![body.to_owned()];
    }
    let mut chunks = VecDeque::from(text_chunks(body, body_limit));
    let mut fitted = Vec::new();
    while let Some(chunk) = chunks.pop_front() {
        if fenced_chunk(opening, &chunk, closing).chars().count() <= limit {
            fitted.push(chunk);
            continue;
        }
        let reduced_limit = body_limit.saturating_sub(1);
        if reduced_limit == 0 {
            fitted.push(chunk);
            continue;
        }
        let reduced = text_chunks(&chunk, reduced_limit);
        for piece in reduced.into_iter().rev() {
            chunks.push_front(piece);
        }
    }
    fitted
}

/// Reconstructs one complete fenced code block from one body chunk.
fn fenced_chunk(opening: &str, body: &str, closing: &str) -> String {
    let mut output = String::with_capacity(opening.len() + body.len() + closing.len() + 1);
    output.push_str(opening);
    output.push_str(body);
    if !body.ends_with(['\r', '\n']) {
        output.push('\n');
    }
    output.push_str(closing);
    output
}

/// Checks a concatenation using Unicode character counts.
fn fits(current: &str, addition: &str, limit: usize) -> bool {
    current
        .chars()
        .count()
        .saturating_add(addition.chars().count())
        <= limit
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

    /// Verifies replayed ACP user messages become normal user projections.
    #[test]
    fn replayed_user_chunks_render_as_user_messages() {
        let mut replay = event(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("hello from history")))
                .message_id(AcpMessageId::new("user-1")),
        ));
        replay.replay = true;
        let ProjectionOutcome::Updated(state) = reduce(None, &replay).unwrap() else {
            panic!("replayed user text should change the projection");
        };
        assert_eq!(state.source_kind, "user_message");
        assert_eq!(state.source_id, "user-1");
        assert_eq!(
            render_projection(&state).unwrap(),
            vec!["hello from history"]
        );
    }

    /// Verifies live user echoes remain invisible because the gateway message
    /// is already present in Discord.
    #[test]
    fn live_user_chunks_are_ignored() {
        assert_eq!(
            reduce(
                None,
                &event(SessionUpdate::UserMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new("already visible")))
                        .message_id(AcpMessageId::new("user-1")),
                )),
            )
            .unwrap(),
            ProjectionOutcome::Ignored
        );
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

    /// Renders read tool updates as a path-only call without returning content.
    #[test]
    fn read_tool_calls_render_as_path_only() {
        let call = event(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "src/main.rs")
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
        assert_eq!(rendered, "📖 **read** `src/main.rs`");
    }

    /// Keeps the tool name while omitting a title that only repeats it.
    #[test]
    fn redundant_tool_titles_are_omitted() {
        let state = serde_json::json!({
            "toolCallId": "tool-1",
            "title": "Switch_Mode",
            "kind": "switch_mode",
            "status": "pending"
        });
        assert_eq!(render_tool_text(&state), "🔁 **switch mode** · *pending*");
    }

    /// Omits an execute title when it repeats the command in the shell block.
    #[test]
    fn execute_titles_matching_commands_are_omitted() {
        let state = serde_json::json!({
            "toolCallId": "tool-1",
            "title": "cargo test",
            "kind": "execute",
            "status": "completed",
            "rawInput": {"command": "cargo test"},
            "rawOutput": {"stdout": "test output", "stderr": "terminal output"}
        });
        assert_eq!(
            render_tool_text(&state),
            "⚙️ **execute**\n```sh\ncargo test\n```"
        );
    }

    /// Omits an execute title when it repeats the command with a shell prompt.
    #[test]
    fn execute_titles_with_shell_prompt_are_omitted() {
        let state = serde_json::json!({
            "toolCallId": "tool-1",
            "title": "$ cargo test",
            "kind": "execute",
            "status": "completed",
            "rawInput": {"command": "cargo test"},
            "rawOutput": {"stdout": "test output"}
        });
        assert_eq!(
            render_tool_text(&state),
            "⚙️ **execute**\n```sh\ncargo test\n```"
        );
    }

    /// Retains an execute title when it describes a different command.
    #[test]
    fn execute_titles_different_from_commands_are_shown() {
        let state = serde_json::json!({
            "toolCallId": "tool-1",
            "title": "run tests",
            "kind": "execute",
            "status": "completed",
            "rawInput": {"command": "cargo test"}
        });
        assert_eq!(
            render_tool_text(&state),
            "⚙️ **execute** `run tests`\n```sh\ncargo test\n```"
        );
    }

    /// Omits search results while retaining the query in the tool header.
    #[test]
    fn search_tool_calls_render_without_results() {
        let state = serde_json::json!({
            "toolCallId": "tool-1",
            "title": "src/**/*.rs",
            "kind": "search",
            "status": "completed",
            "content": [{"type": "text", "text": "search results"}],
            "rawOutput": {"matches": ["src/main.rs"]}
        });
        assert_eq!(render_tool_text(&state), "🔍 **search** `src/**/*.rs`");
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

    /// Keeps an edit header with its diff and repeats fences when splitting.
    #[test]
    fn fenced_code_chunks_keep_their_language_and_delimiters() {
        let fence = char::from(96).to_string().repeat(3);
        let value = format!("edit blah\n{fence}diff\n+ one\n- two\n+ three\n{fence}");
        assert_eq!(
            split_message(&value, value.chars().count()),
            vec![value.clone()]
        );
        assert_eq!(
            split_message(&value, 30),
            vec![
                format!("edit blah\n{fence}diff\n+ one\n{fence}"),
                format!("{fence}diff\n- two\n+ three\n{fence}")
            ]
        );
    }

    /// Keeps split thought code blocks inside the existing italic wrapper.
    #[test]
    fn thought_code_chunks_keep_italic_wrappers() {
        let fence = char::from(96).to_string().repeat(3);
        let body = "line\n".repeat(500);
        let value = format!("{fence}rust\n{body}{fence}");
        let chunks = render_text(&serde_json::json!({"text": value}), true);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| {
            chunk.starts_with('*') && chunk.ends_with('*') && chunk.matches(&fence).count() == 2
        }));
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

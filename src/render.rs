//! Ordered ACP updates projected into Discord messages.
//!
//! The reducer in this module is deliberately independent of Discord and
//! persistence. It accepts one ordered update at a time and returns the
//! complete projection for the affected source. The Discord adapter then
//! synchronizes message IDs and persists the result.

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use serde::{Deserialize, Serialize};
use serenity::all::{Context, CreateMessage, EditMessage, GenericChannelId, MessageId};
use text_splitter::{ChunkConfig, MarkdownSplitter};

use crate::{Bot, BotError, BotResult, db::RenderProjection};

/// Discord's maximum normal message length.
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
        SessionUpdate::AgentMessageChunk(chunk) => {
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
    text: String,
}

/// Finds the stable source key for an update.
fn source_key(event: &ProjectionEvent) -> Option<(&'static str, String)> {
    match &event.update {
        SessionUpdate::AgentMessageChunk(chunk) => chunk.message_id.as_ref().map_or_else(
            || (!event.replay).then(|| ("agent_message", format!("turn:{}", event.turn_id))),
            |id| Some(("agent_message", id.to_string())),
        ),
        _ => None,
    }
}

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

fn render_projection(projection: &RenderProjection) -> BotResult<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(&projection.state_json)
        .map_err(|error| BotError::Projection(format!("invalid source state: {error}")))?;
    let text = match projection.source_kind.as_str() {
        "agent_message" => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => return Ok(Vec::new()),
    };
    Ok(split_message(&text, MESSAGE_LIMIT))
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

#[derive(Debug)]
struct SyncFailure {
    message_ids: Vec<MessageId>,
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
        ContentBlock, ContentChunk, MessageId as AcpMessageId, TextContent,
    };
    use serenity::all::{GenericChannelId, MessageId};

    use super::*;

    fn event(update: SessionUpdate) -> ProjectionEvent {
        ProjectionEvent {
            thread_id: GenericChannelId::new(1),
            turn_id: "2".into(),
            replay: false,
            update,
        }
    }

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

    #[test]
    fn message_chunks_are_unicode_safe_and_bounded() {
        let chunks = split_message(&"🙂".repeat(20), 7);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 7));
        assert_eq!(chunks.concat(), "🙂".repeat(20));
    }

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

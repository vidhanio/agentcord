use std::{fmt::Write, sync::Arc};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use serenity::all::{
    Colour, Context, CreateEmbed, CreateMessage, EditMessage, GenericChannelId, MessageId,
};

use crate::{Bot, BotResult, db::RenderRow};

const MESSAGE_LIMIT: usize = 2000;
const THOUGHT_LIMIT: usize = 1900;
const EMBED_LIMIT: usize = 3800;

#[derive(Default, Deserialize, Serialize)]
struct OutputState {
    thought: String,
    final_text: String,
    thought_message_count: usize,
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
                    self.render_tool(ctx, thread, call, None).await?;
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
            if thought {
                if state.final_text.is_empty() {
                    state.thought.push_str(&text);
                    state.thought = keep_tail(&state.thought, THOUGHT_LIMIT);
                    changed = true;
                }
            } else {
                state.final_text.push_str(&text);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        if state.final_text.is_empty() {
            let thought_count = state
                .thought_message_count
                .min(row.discord_message_ids.len());
            let mut thought_ids = row.discord_message_ids[..thought_count].to_vec();
            sync_text_messages(
                ctx,
                thread,
                &mut thought_ids,
                &[format!("*{}*", state.thought)],
            )
            .await?;
            row.discord_message_ids = thought_ids;
            state.thought_message_count = row.discord_message_ids.len();
        } else {
            if state.thought_message_count == 0 && !state.thought.is_empty() {
                state.thought_message_count = row.discord_message_ids.len().min(1);
            }
            let thought_count = state
                .thought_message_count
                .min(row.discord_message_ids.len());
            let mut final_ids = row.discord_message_ids.split_off(thought_count);
            let final_chunks = split_message(&state.final_text, MESSAGE_LIMIT);
            sync_text_messages(ctx, thread, &mut final_ids, &final_chunks).await?;
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
        existing: Option<RenderRow>,
    ) -> BotResult {
        let call_id = call.tool_call_id.to_string();
        let mut row = existing.unwrap_or_else(|| RenderRow {
            source_key: format!("tool:{call_id}"),
            kind: "tool".into(),
            discord_message_ids: vec![],
            state_json: "{}".into(),
        });
        let value = serde_json::to_value(call).unwrap_or_default();
        row.state_json = serde_json::to_string(&value).expect("tool call serializes");
        sync_tool_message(ctx, thread, &mut row.discord_message_ids, &value).await?;
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
        sync_tool_message(ctx, thread, &mut row.discord_message_ids, &state).await?;
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
                .edit_message(&ctx.http, id, EditMessage::new().content(chunk))
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
    Ok(())
}

async fn sync_tool_message(
    ctx: &Context,
    thread: GenericChannelId,
    ids: &mut Vec<MessageId>,
    state: &serde_json::Value,
) -> BotResult {
    let kind = state
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("other");
    if matches!(kind, "edit" | "execute") {
        let content = if kind == "edit" {
            render_diff(state)
        } else {
            render_command(state)
        };
        return sync_text_messages(ctx, thread, ids, &[content]).await;
    }
    let embed = tool_embed(state);
    if let Some(id) = ids.first().copied()
        && thread
            .edit_message(&ctx.http, id, EditMessage::new().embed(embed.clone()))
            .await
            .is_ok()
    {
        return Ok(());
    }
    let message = thread
        .send_message(&ctx.http, CreateMessage::new().embed(embed))
        .await?;
    if ids.is_empty() {
        ids.push(message.id);
    } else {
        ids[0] = message.id;
    }
    Ok(())
}

fn tool_embed(state: &serde_json::Value) -> CreateEmbed<'static> {
    let title = state
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool call");
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let colour = match status {
        "completed" => Colour::from_rgb(87, 242, 135),
        "failed" => Colour::from_rgb(237, 66, 69),
        "in_progress" => Colour::from_rgb(254, 231, 92),
        _ => Colour::LIGHT_GREY,
    };
    let mut details = state.clone();
    if let Some(object) = details.as_object_mut() {
        object.remove("title");
        object.remove("toolCallId");
        object.remove("_meta");
    }
    CreateEmbed::new()
        .title(format!("{title} · {status}"))
        .description(format!(
            "```json\n{}\n```",
            cap(
                &serde_json::to_string_pretty(&details).unwrap_or_default(),
                EMBED_LIMIT
            )
        ))
        .colour(colour)
}

fn render_command(state: &serde_json::Value) -> String {
    let command = state
        .get("rawInput")
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| {
            state
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("command")
        });
    let status = state
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    format!(
        "**{status}**\n```sh\n{}\n```",
        cap(&command.replace("```", "`\u{200b}``"), 1900)
    )
}

fn render_diff(state: &serde_json::Value) -> String {
    let mut body = String::new();
    if let Some(contents) = state.get("content").and_then(serde_json::Value::as_array) {
        for diff in contents.iter().filter(|content| {
            content.get("type").and_then(serde_json::Value::as_str) == Some("diff")
        }) {
            let path = diff
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("file");
            let _ = write!(body, "--- a/{path}\n+++ b/{path}\n");
            if let Some(old) = diff.get("oldText").and_then(serde_json::Value::as_str) {
                for line in old.lines() {
                    let _ = writeln!(body, "-{line}");
                }
            }
            if let Some(new) = diff.get("newText").and_then(serde_json::Value::as_str) {
                for line in new.lines() {
                    let _ = writeln!(body, "+{line}");
                }
            }
        }
    }
    if body.is_empty() {
        body = serde_json::to_string_pretty(state.get("rawInput").unwrap_or(state))
            .unwrap_or_default();
    }
    format!(
        "```diff\n{}\n```",
        cap(&body.replace("```", "`\u{200b}``"), 1988)
    )
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

fn keep_tail(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let marker = "… earlier thought omitted …\n";
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

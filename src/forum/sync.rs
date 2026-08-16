//! Transcript mirroring: cursor-based syncs of session transcripts into
//! their forum posts — agent turns as plain messages, tool calls as
//! single-argument text or stateful embeds (errors baked into the call's
//! own message), user turns as webhook echoes (see [`super::echo`]).

use std::path::Path;

use serenity::all::{ChannelId, Context, CreateMessage, EditMessage, MessageId};
use tracing::warn;

use crate::{
    BotResult,
    db::SessionRow,
    error::BotError,
    forum::{Forum, from_i64, render, titles},
    herdr::{Agent, SessionPath},
    session::{Harness, SessionMessage, SessionRole, ToolCall, read_session_messages},
};

impl Forum {
    /// Syncs the session's transcript into its forum post: new agent turns
    /// are posted as plain messages, tool calls as single-argument text or
    /// stateful embeds (posted once, edited in place as they complete,
    /// errors baked into the call's own message), and new user
    /// turns as echoes (skipped when the user already typed them). A
    /// backlog beyond
    /// [`crate::config::CATCHUP_BACKLOG`] messages (downtime, reconnect, a
    /// transcript rotation) truncates to the last
    /// [`crate::config::MAX_SYNC_MESSAGES`] messages, announced in small
    /// italic text; normal turns — even heavy tool-call turns — are
    /// mirrored whole. The cursor commits after every post, so a mid-sync
    /// failure (e.g. a rate limit) resumes from the last posted message
    /// instead of re-posting it.
    pub async fn sync_session(
        &self,
        ctx: &Context,
        session: &SessionRow,
        harness: Harness,
        forum: ChannelId,
    ) -> BotResult<()> {
        // Syncs from the poll and the event loop can race on the cursor:
        // serialize them AND re-read the row under the lock — a caller's
        // copy may predate another sync's commits, and a stale cursor
        // would re-post the same window.
        let _sync = self.sync_lock.lock().await;
        let session = self
            .db
            .get_session(&SessionPath::from(session.session_path.as_str()))
            .await?
            .unwrap_or_else(|| session.clone());
        let Some(post_id) = session.post_channel_id else {
            return Ok(());
        };
        let post = from_i64(post_id)?;

        let messages = match read_session_messages(harness, &session.transcript_path) {
            Ok(messages) => messages,
            // The source may be missing: the transcript can be mid-rotation
            // (omp rewrites it via a delete+recreate dance), or the opencode
            // store may not exist yet. Retry on the next trigger instead of
            // aborting the sync.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(BotError::Other(format!(
                    "failed to read session `{}`: {error}",
                    session.session_path
                )));
            }
        };

        // The cursor is the number of transcript messages already consumed;
        // clamp it to the file length in case the file was truncated. A
        // backlog beyond the catch-up magnitude is truncated to the last
        // few messages and announced, so a stale backlog can never flood
        // the thread.
        let synced = usize::try_from(session.synced_messages)
            .unwrap_or(0)
            .min(messages.len());
        let pending = messages.len() - synced;
        let skip = if pending > crate::config::CATCHUP_BACKLOG {
            pending.saturating_sub(crate::config::MAX_SYNC_MESSAGES)
        } else {
            0
        };
        if skip > 0
            && let Err(error) = post
                .widen()
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!(
                        "-# *{skip} older messages omitted during catch-up*"
                    )),
                )
                .await
        {
            warn!(
                ?error,
                session = %session.session_path,
                "failed to announce omitted messages"
            );
        }
        let mut consumed = synced + skip;
        let mut last_id = session.last_discord_message_id;

        for message in messages.iter().skip(synced + skip) {
            consumed += 1;
            let message_id = match message.role {
                SessionRole::Agent => {
                    Some(self.post_agent_message(ctx, post, &message.text).await?)
                }
                SessionRole::Tool => self.sync_tool_embed(ctx, post, &session, message).await?,
                SessionRole::User => {
                    self.post_user_echo(ctx, forum, post, last_id, &message.text)
                        .await?
                }
            };
            let Some(message_id) = message_id else {
                // Not a new bot post: the user already typed this message,
                // or a tool embed was posted/edited in place on an earlier
                // sync and was already counted. The cursor stalls until a
                // later message is posted.
                continue;
            };
            last_id = Some(crate::forum::to_i64(message_id)?);
            let updated = SessionRow {
                workspace_label: session.workspace_label.clone(),
                session_path: session.session_path.clone(),
                cwd: session.cwd.clone(),
                transcript_path: session.transcript_path.clone(),
                post_channel_id: session.post_channel_id,
                synced_messages: i64::try_from(consumed)
                    .map_err(|_| BotError::Other("synced message count overflow".into()))?,
                last_discord_message_id: last_id,
                starter_message_id: session.starter_message_id,
            };
            self.db.upsert_session(&updated).await?;
        }

        // Completion pass: calls posted on an earlier sync (already past the
        // cursor) may have completed since — re-parsing the whole file gives
        // their messages a new state, so edit the posted embeds in place to
        // match. Calls not yet posted are handled by the cursor loop above.
        for message in messages
            .iter()
            .filter(|message| message.role == SessionRole::Tool)
        {
            let Some(call) = &message.tool else {
                continue;
            };
            let key = (
                SessionPath::from(session.session_path.clone()),
                call.call_id.clone(),
            );
            if self
                .tool_messages
                .lock()
                .expect("tool_messages lock poisoned")
                .contains_key(&key)
            {
                self.sync_tool_embed(ctx, post, &session, message).await?;
            }
        }

        Ok(())
    }

    /// Best-effort mirror of the session at `session_path`, used by the
    /// relay after a prompt and by the poll on its tick. When the post or
    /// its forum was deleted on Discord, escalates to the full live-agent
    /// sync (ensure + metadata + mirror). Failures are logged, not
    /// propagated.
    pub async fn sync_session_by_path(&self, ctx: &Context, session_path: &SessionPath) {
        let session = match self.db.get_session(session_path).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                warn!(%session_path, "mirror requested for unknown session");
                return;
            }
            Err(error) => {
                warn!(%session_path, ?error, "failed to look up session for mirror");
                return;
            }
        };
        let Some(post_id) = session.post_channel_id else {
            return;
        };
        let Ok(post) = from_i64(post_id) else {
            warn!(%session_path, "invalid post binding");
            return;
        };
        let Ok(forum) = self.forum_for_post(ctx, post).await else {
            // The post or its forum was deleted on Discord: the full
            // live-agent sync re-creates the workspace forum and the post.
            self.recover_session(ctx, &session).await;
            return;
        };
        self.mirror_session_inner(ctx, &session, forum).await;
    }

    /// Mirrors one session's transcript into its post: resolves the
    /// harness from the live agent (omp when there is none) and runs the
    /// cursor-based sync. No recovery escalation — callers that can repair
    /// a deleted post do that first.
    async fn mirror_session_inner(&self, ctx: &Context, session: &SessionRow, forum: ChannelId) {
        if let Err(error) = self
            .sync_session(
                ctx,
                session,
                self.live_agent_harness(session)
                    .await
                    .unwrap_or(Harness::Omp),
                forum,
            )
            .await
        {
            warn!(session = %session.session_path, ?error, "session mirror failed");
        }
    }

    /// Re-creates a live session's forum post (and its workspace forum)
    /// after a deletion on Discord: the metadata pass (ensure + tags +
    /// title), then the mirror straight into the re-created post — not via
    /// [`Self::sync_session_by_path`], whose recovery branch would recurse
    /// back here.
    async fn recover_session(&self, ctx: &Context, session: &SessionRow) {
        let Some(agent) = self.live_agent(session).await else {
            warn!(
                session = %session.session_path,
                "cannot recover session post: no live agent"
            );
            return;
        };
        self.sync_agent_post(ctx, &agent).await;
        let Ok(Some(session)) = self
            .db
            .get_session(&SessionPath::from(session.session_path.clone()))
            .await
        else {
            return;
        };
        let Some(post_id) = session.post_channel_id else {
            return;
        };
        let Ok(post) = from_i64(post_id) else {
            return;
        };
        let Ok(forum) = self.forum_for_post(ctx, post).await else {
            return;
        };
        self.mirror_session_inner(ctx, &session, forum).await;
    }

    /// Every live herdr agent hosting `session`: matches each agent's
    /// reported session value against the row's key and its adopted
    /// transcript.
    pub async fn hosting_agents(&self, session: &SessionRow) -> Vec<Agent> {
        self.herdr
            .list_agents()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|agent| {
                agent
                    .agent_session
                    .as_ref()
                    .is_some_and(|agent_session| session.hosts(agent_session.value.as_str()))
            })
            .collect()
    }

    /// The live herdr agent hosting `session`, if any.
    async fn live_agent(&self, session: &SessionRow) -> Option<Agent> {
        self.hosting_agents(session).await.into_iter().next()
    }

    /// The harness of the live agent hosting `session`, if any.
    pub async fn live_agent_harness(&self, session: &SessionRow) -> Option<Harness> {
        self.live_agent(session)
            .await
            .and_then(|agent| agent.harness)
    }

    /// Syncs a live agent's post metadata: ensures the post + row (the
    /// re-created one when it was deleted on Discord), re-applies the
    /// harness/status tags and the transcript-sourced post title, and
    /// reopens an archived thread. The transcript mirror is the poll's job
    /// (and the relay settle's) — this pass never reads the transcript.
    pub async fn sync_agent_post(&self, ctx: &Context, agent: &Agent) {
        if let Err(error) = self.ensure_session_post(ctx, agent).await {
            warn!(?error, pane = %agent.pane_id, "failed to ensure session post");
            return;
        }
        let Some(session) = self.session_for_agent(agent).await else {
            return;
        };

        let harness = agent.harness.unwrap_or(Harness::Omp);
        let Some(post_id) = session.post_channel_id else {
            return;
        };
        let post = match from_i64(post_id) {
            Ok(post) => post,
            Err(error) => {
                warn!(?error, pane = %agent.pane_id, "invalid post binding");
                return;
            }
        };
        let Ok(forum) = self.forum_for_post(ctx, post).await else {
            warn!(pane = %agent.pane_id, "failed to resolve post forum");
            return;
        };
        let title = titles::session_title(agent, harness, Path::new(&session.transcript_path));
        if let Err(error) = self
            .update_post_metadata(
                ctx,
                forum,
                post,
                Some(harness),
                agent.status(),
                Some(&title),
            )
            .await
        {
            warn!(?error, pane = %agent.pane_id, "failed to update post metadata");
        }
    }

    /// Refreshes a session post's starter message with `intro`. Skipped
    /// when the starter id is unknown (rows predating the capture); message
    /// edits are silent, so the refresh runs unconditionally on metadata
    /// updates and on session death.
    pub async fn refresh_starter(
        &self,
        ctx: &Context,
        session: &SessionRow,
        post: ChannelId,
        intro: String,
    ) -> BotResult<()> {
        let Some(starter_id) = session.starter_message_id else {
            return Ok(());
        };
        let mut starter = ctx
            .http
            .get_message(
                post.widen(),
                MessageId::new(
                    u64::try_from(starter_id)
                        .map_err(|_| BotError::Other("invalid starter message id".into()))?,
                ),
            )
            .await?;
        starter
            .edit(ctx, EditMessage::new().content(intro).embeds(Vec::new()))
            .await?;
        Ok(())
    }

    /// Refreshes a session post's starter message with the agent's current
    /// intro (pane · worktree · cwd · session). Only called when the intro's
    /// contents can change: post creation (built inline), session death
    /// (`inactive`), and resume (a new pane). The steady state never
    /// rewrites it.
    pub async fn refresh_agent_intro(
        &self,
        ctx: &Context,
        session: &SessionRow,
        post: ChannelId,
        agent: &Agent,
    ) -> BotResult<()> {
        let worktree = match self.workspace_by_id(&agent.workspace_id).await {
            Ok(Some(workspace)) => self.worktree_branch(&workspace).await,
            _ => None,
        };
        self.refresh_starter(
            ctx,
            session,
            post,
            titles::session_intro(
                Some(agent),
                worktree.as_deref(),
                &agent.cwd,
                agent.agent_session.as_ref().map(|session| &session.value),
            ),
        )
        .await
    }

    /// Posts an agent turn as plain Discord messages, splitting text longer
    /// than Discord's limit at line boundaries. Returns the last posted
    /// message id.
    async fn post_agent_message(
        &self,
        ctx: &Context,
        post: ChannelId,
        text: &str,
    ) -> BotResult<MessageId> {
        let mut last: Option<MessageId> = None;
        for chunk in render::split_lines(text, serenity::constants::MESSAGE_CODE_LIMIT) {
            last = Some(
                post.widen()
                    .send_message(&ctx.http, CreateMessage::new().content(chunk))
                    .await?
                    .id,
            );
        }
        last.ok_or_else(|| BotError::Other("empty agent message".into()))
    }

    /// Posts a tool call's message, or updates the already-posted message
    /// in place when the call's state changed since it was last posted: a
    /// call is posted exactly once — single-argument calls as plain text
    /// (gear while running, red X and a code-block error once failed),
    /// the rest as stateful embeds carrying an error field — and later
    /// completions edit that same message. Returns the posted message id
    /// for a fresh post, and `None` for an in-place edit or a repeat of an
    /// unchanged state — the cursor treats `None` like a user-echo skip
    /// (the message was already counted).
    async fn sync_tool_embed(
        &self,
        ctx: &Context,
        post: ChannelId,
        session: &SessionRow,
        message: &SessionMessage,
    ) -> BotResult<Option<MessageId>> {
        let Some(call) = &message.tool else {
            // Tool messages without structured call data keep the old
            // code-block fallback.
            let id = post
                .widen()
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!(
                        "```\n{}\n```",
                        message.text.replace("```", "`\u{200b}``")
                    )),
                )
                .await?
                .id;
            return Ok(Some(id));
        };

        let key = (
            SessionPath::from(session.session_path.clone()),
            call.call_id.clone(),
        );
        // The lock is scoped to this statement so no guard lives across the
        // awaits below.
        let posted = self
            .tool_messages
            .lock()
            .expect("tool_messages lock poisoned")
            .get(&key)
            .copied();

        let Some((message_id, posted_state)) = posted else {
            // First sight of this call: post it and remember it.
            let id = self.post_tool_call(ctx, post, call).await?;
            self.tool_messages
                .lock()
                .expect("tool_messages lock poisoned")
                .insert(key, (id, call.state));
            return Ok(Some(id));
        };

        if posted_state == call.state {
            // The message already shows this state; nothing to post.
            return Ok(None);
        }

        // The call completed since it was posted: refresh the message in
        // place — the embed recolours, the text form swaps its gear for
        // the failure X and appends the error block.
        self.update_tool_call(ctx, post, message_id, call).await?;
        self.tool_messages
            .lock()
            .expect("tool_messages lock poisoned")
            .insert(key, (message_id, call.state));
        Ok(None)
    }

    /// Posts a tool call's message: the single-argument text form when the
    /// call takes exactly one argument, else the stateful embed. Returns
    /// the posted message id.
    async fn post_tool_call(
        &self,
        ctx: &Context,
        post: ChannelId,
        call: &ToolCall,
    ) -> BotResult<MessageId> {
        let message = render::tool_call_text(call).map_or_else(
            || CreateMessage::new().embed(render::tool_embed(call)),
            |text| CreateMessage::new().content(text),
        );
        Ok(post.widen().send_message(&ctx.http, message).await?.id)
    }

    /// Edits a posted tool call's message in place for its new state: the
    /// gear comes off the text form (or the failure X and error block
    /// appear), the embed recolours.
    async fn update_tool_call(
        &self,
        ctx: &Context,
        post: ChannelId,
        message_id: MessageId,
        call: &ToolCall,
    ) -> BotResult<()> {
        let edit = render::tool_call_text(call).map_or_else(
            || EditMessage::new().embed(render::tool_embed(call)),
            |text| EditMessage::new().content(text),
        );
        post.widen()
            .edit_message(&ctx.http, message_id, edit)
            .await?;
        Ok(())
    }
}

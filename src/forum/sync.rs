//! Transcript mirroring: cursor-based syncs of session transcripts into
//! their forum posts — agent turns as plain messages, tool calls as
//! single-argument text or stateful embeds (errors baked into the call's
//! own message), user turns as webhook echoes.

use std::path::Path;

use serenity::all::{
    ChannelId, Context, CreateForumPost, CreateMessage, CreateWebhook, EditMessage, ExecuteWebhook,
    GetMessages, MessageId, Webhook,
};
use tracing::{info, warn};

use crate::{
    BotResult,
    db::SessionRow,
    error::BotError,
    forum::{
        Forum, from_i64,
        titles::{session_intro, session_title},
        to_i64,
    },
    herdr::{Agent, SessionPath},
    session::{AgentKind, SessionMessage, SessionRole, ToolCall, read_session},
    utils::split_lines,
};

/// The allowed user's Discord profile, used to name and avatar the webhook
/// that mirrors their turns into session posts.
#[derive(Debug, Clone)]
struct UserProfile {
    username: String,
    avatar_url: Option<String>,
}

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
    pub(crate) async fn sync_session(
        &self,
        ctx: &Context,
        session: &SessionRow,
        kind: AgentKind,
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

        let messages = match read_session(kind, Path::new(&session.transcript_path)) {
            Ok(messages) => messages,
            // The transcript may be mid-rotation (omp rewrites it via a
            // delete+recreate dance); retry on the next trigger instead of
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
                .send_message(
                    ctx,
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
            last_id = Some(to_i64(message_id)?);
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

    /// Best-effort sync of the session at `session_path`, used by the relay
    /// after a prompt and by the poll on its tick. Failures are logged, not
    /// propagated.
    pub async fn sync_session_by_path(&self, ctx: &Context, session_path: &SessionPath) {
        let session = match self.db.get_session(session_path).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                warn!(%session_path, "sync requested for unknown session");
                return;
            }
            Err(error) => {
                warn!(%session_path, ?error, "failed to look up session for sync");
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
        if let Err(error) = self
            .sync_session(
                ctx,
                &session,
                self.live_agent_kind(&session)
                    .await
                    .unwrap_or(AgentKind::Omp),
                forum,
            )
            .await
        {
            warn!(%session_path, ?error, "session sync failed");
        }
    }

    /// Re-creates a live session's forum post (and its workspace forum)
    /// after a deletion on Discord, through the full live-agent sync.
    async fn recover_session(&self, ctx: &Context, session: &SessionRow) {
        let Some(agent) = self.live_agent(session).await else {
            warn!(
                session = %session.session_path,
                "cannot recover session post: no live agent"
            );
            return;
        };
        self.sync_agent_session(ctx, &agent).await;
    }

    /// The live herdr agent hosting `session`, if any. Matches the agent's
    /// reported session value against the row's key and its adopted
    /// transcript.
    async fn live_agent(&self, session: &SessionRow) -> Option<Agent> {
        let agents = self.herdr.list_agents().await.ok()?;
        agents.into_iter().find(|agent| {
            agent
                .agent_session
                .as_ref()
                .is_some_and(|agent_session| session.hosts(agent_session.value.as_str()))
        })
    }

    /// The agent kind of the live agent hosting `session`, if any.
    pub(crate) async fn live_agent_kind(&self, session: &SessionRow) -> Option<AgentKind> {
        self.live_agent(session)
            .await
            .and_then(|agent| AgentKind::parse(agent.agent.as_deref().unwrap_or("")))
    }

    /// Creates a forum post for a brand-new session and inserts its
    /// database row.
    pub(crate) async fn create_session_post(
        &self,
        ctx: &Context,
        agent: &Agent,
        kind: AgentKind,
        session_path: &SessionPath,
    ) -> BotResult<()> {
        let Some(workspace) = self.workspace_by_id(&agent.workspace_id).await? else {
            warn!(workspace_id = %agent.workspace_id, "agent workspace not found, skipping session post");
            return Ok(());
        };
        let forum = self.ensure_workspace_forum(ctx, &workspace).await?;

        // The title reads the transcript the row syncs — the adopted file
        // when the row rotated.
        let existing = self.db.get_session(session_path).await?;
        let transcript = existing.as_ref().map_or_else(
            || session_path.to_string(),
            |row| row.transcript_path.clone(),
        );
        let intro_path = agent
            .agent_session
            .as_ref()
            .map_or(session_path, |session| &session.value);
        let worktree = self.worktree_branch(&workspace).await;
        let intro = session_intro(
            Some(agent),
            worktree.as_deref(),
            &agent.cwd,
            Some(intro_path),
        );
        let title = session_title(agent, kind, Path::new(&transcript));
        let created = forum
            .create_forum_post(
                ctx,
                CreateForumPost::new(title, CreateMessage::new().content(intro)),
            )
            .await?;
        let post = created.id;
        // A fresh thread's last message is its starter (the intro); keep its
        // id so the intro can be refreshed as post metadata.
        let starter_message_id = created.last_message_id.map(to_i64).transpose()?;

        // Re-bind the existing row (keyed `session_path`, possibly an
        // adopted transcript) or create it; the cursor restarts on the
        // fresh post.
        let row = match existing {
            Some(mut row) => {
                row.post_channel_id = Some(to_i64(post)?);
                row.synced_messages = 0;
                row.last_discord_message_id = None;
                row.starter_message_id = starter_message_id;
                row
            }
            None => SessionRow {
                workspace_label: workspace.label.clone(),
                session_path: session_path.to_string(),
                cwd: agent.cwd.to_string_lossy().into_owned(),
                transcript_path: session_path.to_string(),
                post_channel_id: Some(to_i64(post)?),
                synced_messages: 0,
                last_discord_message_id: None,
                starter_message_id,
            },
        };
        self.db.upsert_session(&row).await?;

        info!(%session_path, ?post, "bound session to forum post");

        Ok(())
    }

    /// Syncs a live agent into its post: ensures the post + row, re-applies
    /// kind/status tags and the transcript-sourced post title, and mirrors
    /// the transcript.
    pub(crate) async fn sync_agent_session(&self, ctx: &Context, agent: &Agent) {
        if let Err(error) = self.ensure_session_post(ctx, agent).await {
            warn!(?error, pane = %agent.pane_id, "failed to ensure session post");
            return;
        }
        let Some(session) = self.session_for_agent(agent).await else {
            return;
        };

        let kind = AgentKind::parse(agent.agent.as_deref().unwrap_or("")).unwrap_or(AgentKind::Omp);
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
        let title = session_title(agent, kind, Path::new(&session.transcript_path));
        if let Err(error) = self
            .update_post_metadata(ctx, forum, post, Some(kind), agent.status(), Some(&title))
            .await
        {
            warn!(?error, pane = %agent.pane_id, "failed to update post metadata");
        }

        // The post's first message is metadata too — it carries the pane
        // and cwd, which the tags and title do not.
        if let Err(error) = self.sync_post_intro(ctx, &session, post, agent).await {
            warn!(?error, pane = %agent.pane_id, "failed to refresh session intro");
        }

        if let Err(error) = self.sync_session(ctx, &session, kind, forum).await {
            warn!(?error, pane = %agent.pane_id, "failed to sync session");
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
                post,
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
    /// intro (pane · worktree · cwd · session).
    pub async fn sync_post_intro(
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
            session_intro(
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
        for chunk in split_lines(text, serenity::constants::MESSAGE_CODE_LIMIT) {
            last = Some(
                post.send_message(ctx, CreateMessage::new().content(chunk))
                    .await?
                    .id,
            );
        }
        last.ok_or_else(|| BotError::Other("empty agent message".into()))
    }

    /// Posts a user turn as an echo unless the user already typed it into
    /// the thread. Echoes go through the forum's user webhook (named and
    /// avatared after the allowed user) so transcript turns look like the
    /// user's own messages; falls back to a plain bot message when no
    /// webhook is available. Returns the posted message id, or `None` when
    /// the echo was skipped.
    async fn post_user_echo(
        &self,
        ctx: &Context,
        forum: ChannelId,
        post: ChannelId,
        after: Option<i64>,
        text: &str,
    ) -> BotResult<Option<MessageId>> {
        let mut builder = GetMessages::new().limit(100);
        if let Some(after) = after {
            let after = u64::try_from(after)
                .map(MessageId::new)
                .map_err(|_| BotError::Other(format!("{after} is not a valid message id")))?;
            builder = builder.after(after);
        }
        let recent = post.messages(ctx, builder).await?;
        if recent.iter().any(|message| message.content == text) {
            return Ok(None);
        }

        if let Some(profile) = self.user_profile(ctx).await
            && let Some(webhook) = self.user_webhook(ctx, forum).await
        {
            let mut builder = ExecuteWebhook::new()
                .content(text)
                .in_thread(post)
                .username(&profile.username);
            if let Some(avatar_url) = &profile.avatar_url {
                builder = builder.avatar_url(avatar_url.clone());
            }
            match webhook.execute(ctx, true, builder).await {
                Ok(Some(message)) => return Ok(Some(message.id)),
                Ok(None) => warn!("user webhook returned no message, falling back to bot echo"),
                Err(error) => {
                    warn!(?error, "user webhook echo failed, falling back to bot echo");
                }
            }
        }

        let id = post
            .send_message(ctx, CreateMessage::new().content(text))
            .await?
            .id;
        Ok(Some(id))
    }

    /// The allowed user's webhook persona (guild nickname or display name,
    /// plus avatar URL). Fetched fresh on each echo. `None` when no user is
    /// configured or the fetch fails.
    async fn user_profile(&self, ctx: &Context) -> Option<UserProfile> {
        let user_id = self.config.allowed_user_id?;
        let user = ctx.http.get_user(user_id).await.ok()?;
        // The guild nickname takes priority; fall back to the global
        // display name. The "(via herdr)" suffix marks webhook echoes.
        let name = match ctx.http.get_member(self.config.guild_id, user_id).await {
            Ok(member) => member.display_name().to_owned(),
            Err(_) => user.global_name.as_deref().unwrap_or(&user.name).to_owned(),
        };
        Some(UserProfile {
            username: format!("{name} (via herdr)"),
            avatar_url: user.avatar_url(),
        })
    }

    /// The webhook used to mirror the allowed user's turns into `forum`'s
    /// posts: the bot's existing webhook for the channel when there is one
    /// (matched by name), otherwise created on demand. Stateless — listed
    /// fresh on each echo.
    async fn user_webhook(&self, ctx: &Context, forum: ChannelId) -> Option<Webhook> {
        let profile = self.user_profile(ctx).await?;

        let existing = forum
            .webhooks(ctx)
            .await
            .ok()?
            .into_iter()
            .find(|webhook| webhook.name.as_deref() == Some(profile.username.as_str()));
        match existing {
            Some(webhook) => Some(webhook),
            None => match forum
                .create_webhook(ctx, CreateWebhook::new(&profile.username))
                .await
            {
                Ok(webhook) => Some(webhook),
                Err(error) => {
                    warn!(?error, %forum, "failed to create user webhook");
                    None
                }
            },
        }
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
                .send_message(
                    ctx,
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
        let message = crate::utils::tool_call_text(call).map_or_else(
            || CreateMessage::new().embed(crate::utils::tool_embed(call)),
            |text| CreateMessage::new().content(text),
        );
        Ok(post.send_message(ctx, message).await?.id)
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
        let edit = crate::utils::tool_call_text(call).map_or_else(
            || EditMessage::new().embed(crate::utils::tool_embed(call)),
            |text| EditMessage::new().content(text),
        );
        post.edit_message(ctx, message_id, edit).await?;
        Ok(())
    }
}

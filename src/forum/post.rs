//! The session↔post binding lifecycle: ensuring a live agent's session has
//! a forum post (creating post + row on first sight), catching up or
//! deleting new forum posts, and resolving a session's post/harness.

use std::path::Path;

use serenity::all::{
    ChannelId, Context, CreateForumPost, CreateMessage, GetMessages, GuildThread, MessageId,
    PartialGuildThread,
};
use tracing::{info, warn};

use crate::{
    BotResult,
    db::SessionRow,
    forum::{
        Forum, from_i64,
        titles::{session_intro, session_title},
        to_i64,
    },
    herdr::{Agent, SessionPath},
    session::Harness,
};

impl Forum {
    /// Ensures the agent's session has a forum post, creating it (and its
    /// database row) on first sight, and records the pane→session mapping
    /// so a `pane.closed` event can mark the post dead instantly. Agents
    /// without a session reference or with an unknown harness get no post.
    pub async fn ensure_session_post(&self, ctx: &Context, agent: &Agent) -> BotResult<()> {
        let Some(session_path) = agent.agent_session.as_ref().map(|session| &session.value) else {
            return Ok(());
        };

        let Some(harness) = agent.harness else {
            warn!(harness = ?agent.harness, %session_path, "unknown harness, skipping session post");
            return Ok(());
        };

        // Several event paths can race here (a host-post launch, agent
        // detection, the reconcile); without serialization two of them can
        // both see "no row" and create two posts for one session. The
        // syncs share this lock, so this also never interleaves with a
        // cursor commit.
        let _sync = self.sync_lock.lock().await;

        self.sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .insert(agent.pane_id.clone(), session_path.clone());

        // The row keyed by the reported path, or — when herdr has caught up
        // with a rotation the poll already adopted — the row that reads the
        // reported file, so no duplicate post is created.
        let session = if let Some(session) = self.db.get_session(session_path).await? {
            session
        } else {
            let Some(adopted) = self
                .db
                .get_session_by_transcript(session_path.as_str())
                .await?
            else {
                return self
                    .create_session_post(ctx, agent, harness, session_path)
                    .await;
            };
            // Re-map the pane to the adopted row's key so the poll and lock
            // paths resolve it.
            self.sessions_by_pane
                .lock()
                .expect("sessions_by_pane lock poisoned")
                .insert(
                    agent.pane_id.clone(),
                    SessionPath::from(adopted.session_path.clone()),
                );
            adopted
        };

        // Any (re)creation is keyed by the row's own path, so an adopted
        // transcript keeps its row instead of spawning a duplicate.
        let row_key = SessionPath::from(session.session_path.clone());

        let Some(post_id) = session.post_channel_id else {
            return self
                .create_session_post(ctx, agent, harness, &row_key)
                .await;
        };

        // The bound post may have been deleted on Discord (the thread or its
        // forum is gone); a live agent always gets a post, so re-create it
        // then.
        let post = from_i64(post_id)?;
        if !self.channel_exists(ctx, post).await? {
            warn!(%session_path, ?post, "session post deleted, re-creating");
            return self
                .create_session_post(ctx, agent, harness, &row_key)
                .await;
        }
        Ok(())
    }

    /// Creates a forum post for a brand-new session and inserts its
    /// database row.
    pub async fn create_session_post(
        &self,
        ctx: &Context,
        agent: &Agent,
        harness: Harness,
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
        let title = session_title(agent, harness, Path::new(&transcript));
        let created = forum
            .create_forum_post(
                &ctx.http,
                CreateForumPost::new(title, CreateMessage::new().content(intro)),
            )
            .await?;
        let post = created.id;
        // A fresh thread's last message is its starter (the intro); keep its
        // id so the intro can be refreshed as post metadata.
        let starter_message_id = created.base.last_message_id.map(to_i64).transpose()?;

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

    /// Handles a new forum post. The bot's own posts are left alone (the
    /// 2s poll mirrors their transcripts into them); every manual post is
    /// deleted silently — agents launch only through the `/agent` modal.
    pub async fn handle_thread_create(&self, ctx: &Context, thread: &GuildThread) -> BotResult<()> {
        // Managed forums are found through the thread's parent: the forum
        // channel itself, which the workspace row binds.
        let forum_id = thread.parent_id;
        if self
            .db
            .workspace_by_forum(to_i64(forum_id)?)
            .await?
            .is_none()
        {
            // Not a forum channel this bot manages.
            return Ok(());
        }

        // The bot's own fresh posts are not bound yet when the thread-create
        // event arrives; only inspect posts whose starter message isn't ours.
        let messages = thread
            .id
            .widen()
            .messages(
                &ctx.http,
                GetMessages::new().limit(1).after(MessageId::new(1)),
            )
            .await?;
        let Some(starter) = messages.first() else {
            return Ok(());
        };
        if starter.author.id == ctx.cache.current_user().id {
            return Ok(());
        }

        // Manual posts never survive; agents launch only through `/agent`.
        info!(
            thread = %thread.base.name,
            author = ?starter.author.id,
            "deleting manually created forum post"
        );
        thread.id.widen().delete(&ctx.http, None).await?;
        Ok(())
    }

    /// Handles a deleted forum post. A deleted post of a live session is
    /// re-created right away — a live agent always gets its post — via the
    /// recovery pass (ensure + tags + mirror). Manual posts and the bot's
    /// own deletions have no session row and are ignored; a dead session's
    /// deleted post makes the row stale, which the reconcile prunes.
    pub async fn handle_thread_delete(
        &self,
        ctx: &Context,
        thread: &PartialGuildThread,
    ) -> BotResult<()> {
        let post_id = to_i64(thread.id)?;
        let Some(session) = self.db.session_by_post(post_id).await? else {
            return Ok(());
        };
        self.recover_session(ctx, &session).await;
        Ok(())
    }

    /// The harness of the first harness tag applied to `post`, if
    /// any — the harness a dead session's thread keeps carrying.
    pub async fn applied_harness(
        &self,
        ctx: &Context,
        post: ChannelId,
    ) -> BotResult<Option<Harness>> {
        let forum = self.forum_for_post(ctx, post).await?;
        let forum = self.forum_channel(ctx, forum).await?;
        let names = crate::forum::tags::tag_names(&forum);
        let post = self.forum_thread(ctx, post).await?;
        Ok(post
            .applied_tags
            .iter()
            .filter_map(|tag_id| names.get(tag_id).copied())
            .find_map(Harness::parse))
    }

    /// The session row for `agent`, when one exists.
    pub async fn session_for_agent(&self, agent: &Agent) -> Option<SessionRow> {
        let path = agent.agent_session.as_ref().map(|session| &session.value)?;
        self.db.get_session(path).await.ok().flatten()
    }
}

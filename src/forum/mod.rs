//! Forum-side state: per-workspace forum channels, session-bound posts, and
//! the shared sync machinery (see [`sync`], [`events`], [`poll`], and
//! [`titles`] for the per-concern implementations).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use serenity::all::{
    Channel, ChannelId, ChannelType, Context, CreateChannel, CreateForumTag, EditChannel,
    EditThread, ForumEmoji, ForumTag, ForumTagId, GetMessages, GuildChannel, GuildThread,
    MessageId, ReactionType, ThreadId, Typing as TypingHandle, small_fixed_array::TruncatingInto,
};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

use crate::{
    BotResult,
    config::Config,
    db::{Db, SessionRow, WorkspaceRow},
    error::BotError,
    forum::titles::forum_channel_name,
    herdr::{Agent, AgentStatus, Herdr, PaneId, SessionPath, Workspace, WorkspaceId},
    session::{AgentKind, ToolCallId, ToolState},
};

mod events;
mod poll;
mod sync;
mod titles;

/// Emoji for agent-kind tags.
const KIND_EMOJI: &str = "🤖";

/// Discord allows at most 20 tags per forum channel; the bot manages 5
/// status tags plus one tag per agent kind, well under the cap.
const TAG_STATUSES: [(AgentStatus, &str); 5] = [
    (AgentStatus::Idle, "⚪"),
    (AgentStatus::Working, "🟡"),
    (AgentStatus::Blocked, "🔴"),
    (AgentStatus::Done, "🟢"),
    (AgentStatus::Unknown, "⚫"),
];

/// Delay between attempts to (re)establish the herdr event subscription.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);

/// How long a tracked transcript must stay unchanged before the poll
/// suspects the session rotated to a new file.
const SESSION_STALE_GRACE: Duration = Duration::from_secs(300);

/// Converts a forum post title into a valid herdr agent name
/// (`[a-z][a-z0-9_-]{0,31}`): lowercases, turns other characters into `-`,
/// prefixes with `agent-` when it doesn't start with a letter, and truncates
/// to 32 characters. Returns `None` when nothing usable remains.
#[must_use]
pub fn sanitize_agent_name(title: &str) -> Option<String> {
    let mut name = String::with_capacity(title.len());

    for ch in title.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            name.push(ch);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }

    while name.ends_with('-') {
        name.pop();
    }

    if name.is_empty() {
        return None;
    }

    if !name.starts_with(|ch: char| ch.is_ascii_lowercase()) {
        name.insert_str(0, "agent-");
    }

    name.truncate(32);

    while name.ends_with('-') {
        name.pop();
    }

    Some(name)
}

/// A typing indicator on session posts, owned by the event loop: started
/// when an agent starts working, stopped when the entry is dropped. One
/// task per pane, no shared state.
#[derive(Default)]
struct Typing {
    tasks: HashMap<PaneId, TypingHandle>,
}

impl Typing {
    /// Starts the typing indicator for `post` if one is not already
    /// running for this pane.
    fn start(&mut self, ctx: &Context, pane_id: &PaneId, post: ChannelId) {
        if self.tasks.contains_key(pane_id) {
            return;
        }
        self.tasks.insert(
            pane_id.clone(),
            TypingHandle::start(Arc::clone(&ctx.http), post.widen()),
        );
    }
}

/// Posted tool-embed bookkeeping: session path + tool call id → the posted
/// message id and the state it currently shows.
type ToolMessages = Arc<Mutex<HashMap<(SessionPath, ToolCallId), (MessageId, ToolState)>>>;

/// Forum-side state: workspace forums, session-bound posts, and transcript
/// syncing.
#[derive(Debug, Clone)]
pub struct Forum {
    config: Arc<Config>,
    herdr: Herdr,
    db: Db,
    /// pane id → session path, the one piece of in-memory state: it lets a
    /// `pane.closed` event mark the session's post dead instantly, and it
    /// is the poll's live-session set. Everything else is queried live from
    /// herdr or Discord. Entries are removed when a pane closes or
    /// reconcile finds the pane gone.
    sessions_by_pane: Arc<Mutex<HashMap<PaneId, SessionPath>>>,
    /// Sessions currently being resumed, so two messages in a dead thread
    /// cannot launch two agents.
    resuming: Arc<Mutex<HashSet<SessionPath>>>,
    /// Only touched under `sync_lock`; entries are dropped when their
    /// session dies.
    tool_messages: ToolMessages,
    /// Serializes transcript syncs: the poll and the event loop can fire
    /// concurrently, and two syncs reading the same cursor would post
    /// duplicate messages.
    sync_lock: Arc<AsyncMutex<()>>,
}
impl Forum {
    #[must_use]
    pub fn new(config: Arc<Config>, herdr: Herdr, db: Db) -> Self {
        Self {
            config,
            herdr,
            db,
            sessions_by_pane: Arc::new(Mutex::new(HashMap::new())),
            resuming: Arc::new(Mutex::new(HashSet::new())),
            tool_messages: Arc::new(Mutex::new(HashMap::new())),
            sync_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Returns the id of every status and kind tag on `channel_id`, creating
    /// missing tags on demand: lifecycle-status tags get their canonical
    /// emoji, the agent-kind tag gets the kind emoji. Stateless: the forum's
    /// tag list is fetched fresh on each call.
    /// Returns the id of every managed tag on `channel_id`: the lifecycle
    /// statuses and every agent kind, with their canonical emojis. The
    /// forum's tag list is replaced when it differs — missing tags are
    /// created and any tag this bot does not manage is dropped, so a
    /// forum's tags are exactly the bot's (Discord caps tags at 20 per
    /// channel, and the bot manages 8). Stateless: the forum's tag list is
    /// fetched fresh on each call.
    async fn tag_ids(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
    ) -> BotResult<HashMap<String, ForumTagId>> {
        let mut channel = self.forum_channel(ctx, channel_id).await?;

        let desired = TAG_STATUSES
            .iter()
            .map(|(status, emoji)| (status.as_str(), *emoji))
            .chain(
                AgentKind::ALL
                    .iter()
                    .map(|kind| (kind.as_str(), KIND_EMOJI)),
            )
            .collect::<Vec<_>>();
        let managed = desired
            .iter()
            .map(|(name, emoji)| (*name, *emoji))
            .collect::<HashSet<_>>();
        let current = channel
            .available_tags
            .iter()
            .map(|tag| (tag.name.as_str(), tag_emoji(tag)))
            .collect::<HashSet<_>>();

        if current != managed {
            let tags: Vec<CreateForumTag> = desired
                .iter()
                .copied()
                .map(|(name, emoji)| {
                    CreateForumTag::new(name)
                        .emoji(ReactionType::Unicode(emoji.to_owned().trunc_into()))
                })
                .collect();
            channel
                .id
                .edit(&ctx.http, EditChannel::new().available_tags(tags))
                .await?;
            // Re-fetch so the id map below sees the applied tag list.
            channel = self.forum_channel(ctx, channel_id).await?;
        }

        Ok(channel
            .available_tags
            .iter()
            .filter(|tag| managed.contains(&(tag.name.as_str(), tag_emoji(tag))))
            .map(|tag| (tag.name.as_str().to_owned(), tag.id))
            .collect())
    }

    /// Applies `kind` + `status` tags and the post title to a session post
    /// and reopens it: a live agent's post is always open, so an archived
    /// thread (closed on session death, or auto-archived) is unarchived.
    /// Tags are applied unconditionally — herdr is the truth, Discord
    /// mirrors it, and every write is cheap enough to repeat. The title
    /// rename is skipped when it is unchanged: renaming the thread makes
    /// Discord post a channel-name-change message into it, so identical
    /// renames would spam the thread. Post titles come from herdr's raw
    /// terminal title and are renamed in place when the agent's title
    /// changes.
    pub async fn update_post_metadata(
        &self,
        ctx: &Context,
        forum: ChannelId,
        post: ChannelId,
        kind: Option<AgentKind>,
        status: AgentStatus,
        title: Option<&str>,
    ) -> BotResult<()> {
        let ids = self.tag_ids(ctx, forum).await?;

        let mut applied = Vec::new();
        if let Some(kind) = kind
            && let Some(id) = ids.get(kind.as_str())
        {
            applied.push(*id);
        }
        if let Some(id) = ids.get(status.as_str()) {
            applied.push(*id);
        }

        let mut builder = EditThread::new().applied_tags(applied);
        match self.forum_thread(ctx, post).await {
            Ok(thread) => {
                if thread.thread_metadata.archived() {
                    builder = builder.archived(false);
                }
                if let Some(title) = title
                    && thread.base.name.as_str() != title
                {
                    builder = builder.name(title);
                }
            }
            Err(_) => {
                // Unknown thread state; keep the old rename-when-untested
                // behavior and leave the archive state alone.
                if let Some(title) = title {
                    builder = builder.name(title);
                }
            }
        }
        ThreadId::new(post.get()).edit(&ctx.http, builder).await?;

        Ok(())
    }

    /// Whether `channel_id` still exists on Discord; `false` when it was
    /// deleted. Other failures propagate.
    async fn channel_exists(&self, ctx: &Context, channel_id: ChannelId) -> BotResult<bool> {
        match ctx.http.get_channel(channel_id.widen()).await {
            Ok(_) => Ok(true),
            Err(serenity::Error::Http(serenity::all::HttpError::UnsuccessfulRequest(response)))
                if response.status_code == serenity::all::StatusCode::NOT_FOUND =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Returns the forum channel for `workspace`, creating it (and
    /// persisting the mapping) on first use or when the bound forum was
    /// deleted on Discord. Every workspace gets its own forum channel,
    /// created on demand; a worktree workspace mirrors its repo's main
    /// workspace forum.
    pub async fn ensure_workspace_forum(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> BotResult<ChannelId> {
        let workspace = self.forum_workspace(workspace).await?;
        if let Some(row) = self.db.get_workspace(&workspace.label).await?
            && let Some(forum_id) = row.forum_channel_id
        {
            let forum = from_i64(forum_id)?;
            if self.channel_exists(ctx, forum).await? {
                return Ok(forum);
            }
            warn!(workspace = %workspace.label, %forum, "workspace forum deleted, re-creating");
        }

        let name = forum_channel_name(&workspace.label);
        let created = self
            .config
            .guild_id
            .create_channel(&ctx.http, CreateChannel::new(name).kind(ChannelType::Forum))
            .await?;
        self.upsert_forum(&workspace, created.id).await?;
        info!(workspace = %workspace.label, forum = %created.id, "created workspace forum");

        Ok(created.id)
    }

    /// Ensures `workspace`'s forum exists and renames it to match the
    /// workspace's current label (channel name = sanitized label). Called on
    /// workspace events and reconcile, so renames propagate to Discord.
    pub async fn sync_workspace_forum(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> BotResult<()> {
        self.rekey_workspace(workspace).await?;
        // A worktree mirrors its main workspace's forum, which the main
        // workspace's own iteration creates and renames; only a workspace
        // with its own forum is synced here.
        if self.forum_workspace(workspace).await?.workspace_id != workspace.workspace_id {
            return Ok(());
        }
        let forum_id = self.ensure_workspace_forum(ctx, workspace).await?;
        let forum = self.forum_channel(ctx, forum_id).await?;
        let expected = forum_channel_name(&workspace.label);
        if forum.base.name.as_str() != expected {
            forum_id
                .edit(&ctx.http, EditChannel::new().name(expected.clone()))
                .await?;
            info!(workspace = %workspace.label, %expected, "renamed workspace forum");
        }
        Ok(())
    }

    /// Re-keys `workspace`'s row when it is stored under a stale key: rows
    /// from before the label-keyed identity carry the herdr id in the key
    /// position, and a renamed workspace moves its row to the new label
    /// (the stored id identifies it). No-op when the row already matches.
    async fn rekey_workspace(&self, workspace: &Workspace) -> BotResult<()> {
        if let Some(mut row) = self.db.get_workspace(&workspace.label).await? {
            if row.workspace_id != workspace.workspace_id.as_str() {
                row.workspace_id = workspace.workspace_id.to_string();
                self.db.upsert_workspace(&row).await?;
            }
            return Ok(());
        }
        let row = self
            .db
            .get_workspace_by_id(workspace.workspace_id.as_str())
            .await?
            .or(self
                .db
                .get_workspace(workspace.workspace_id.as_str())
                .await?);
        let Some(row) = row else {
            return Ok(());
        };
        self.db.delete_workspace(&row.label).await?;
        self.db
            .upsert_workspace(&WorkspaceRow {
                label: workspace.label.clone(),
                workspace_id: workspace.workspace_id.to_string(),
                forum_channel_id: row.forum_channel_id,
            })
            .await?;
        info!(
            workspace = %workspace.label,
            "re-keyed workspace row to its label"
        );
        Ok(())
    }

    /// Ensures the agent's session has a forum post, creating it (and its
    /// database row) on first sight, and records the pane→session mapping
    /// so a `pane.closed` event can mark the post dead instantly. Agents
    /// without a session reference or with an unknown kind get no post.
    pub async fn ensure_session_post(&self, ctx: &Context, agent: &Agent) -> BotResult<()> {
        let Some(session_path) = agent.agent_session.as_ref().map(|session| &session.value) else {
            return Ok(());
        };

        let Some(kind) = AgentKind::parse(agent.agent.as_deref().unwrap_or("")) else {
            warn!(agent = ?agent.agent, %session_path, "unknown agent kind, skipping session post");
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
                    .create_session_post(ctx, agent, kind, session_path)
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
            return self.create_session_post(ctx, agent, kind, &row_key).await;
        };

        // The bound post may have been deleted on Discord (the thread or its
        // forum is gone); a live agent always gets a post, so re-create it
        // then.
        let post = from_i64(post_id)?;
        if !self.channel_exists(ctx, post).await? {
            warn!(%session_path, ?post, "session post deleted, re-creating");
            return self.create_session_post(ctx, agent, kind, &row_key).await;
        }
        Ok(())
    }

    /// Handles a new forum post. The bot's own posts are left alone (bound
    /// sessions get their transcript caught up); every manual post is
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

        let post_id = to_i64(thread.id)?;
        if let Some(session) = self.db.session_by_post(post_id).await? {
            // Already bound to a session: catch up its transcript. The kind
            // comes from the live agent when there is one, else omp.
            let kind = self
                .live_agent_kind(&session)
                .await
                .unwrap_or(AgentKind::Omp);
            let forum = self
                .forum_for_post(ctx, ChannelId::new(thread.id.get()))
                .await?;
            self.sync_session(ctx, &session, kind, forum).await?;
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

    /// The agent kind of the first agent-kind tag applied to `post`, if
    /// any — the kind a dead session's thread keeps carrying.
    async fn applied_kind(&self, ctx: &Context, post: ChannelId) -> BotResult<Option<AgentKind>> {
        let forum = self.forum_for_post(ctx, post).await?;
        let forum = self.forum_channel(ctx, forum).await?;
        let names = tag_names(&forum);
        let post = self.forum_thread(ctx, post).await?;
        Ok(post
            .applied_tags
            .iter()
            .filter_map(|tag_id| names.get(tag_id).copied())
            .find_map(AgentKind::parse))
    }

    /// A herdr agent name based on `base` that no live agent uses: `base`
    /// itself, or `base-2`, `base-3`, … when taken.
    pub(crate) async fn unique_agent_name(&self, base: &str) -> BotResult<String> {
        let taken = self
            .herdr
            .list_agents()
            .await?
            .into_iter()
            .filter_map(|agent| agent.name)
            .collect::<HashSet<_>>();
        if !taken.contains(base) {
            return Ok(base.to_owned());
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
            suffix += 1;
        }
    }

    /// The working directory for a new agent in `workspace_label`: the cwd
    /// of a live agent in the workspace when there is one, else the cwd of
    /// a previous session, else the user's home directory.
    pub(crate) async fn launch_cwd(&self, workspace_label: &str) -> String {
        // Agents report their workspace by herdr's positional id; match
        // through the workspace list so the identity is the label, the
        // same one the rows use.
        if let (Ok(agents), Ok(workspaces)) = (
            self.herdr.list_agents().await,
            self.herdr.list_workspaces().await,
        ) && let Some(agent) = agents.iter().find(|agent| {
            workspaces.iter().any(|workspace| {
                workspace.workspace_id == agent.workspace_id && workspace.label == workspace_label
            })
        }) {
            return agent.cwd.to_string_lossy().into_owned();
        }
        if let Ok(sessions) = self.db.sessions_by_workspace(workspace_label).await
            && let Some(session) = sessions.first()
        {
            return session.cwd.clone();
        }
        dirs::home_dir().map_or_else(
            || "/tmp".to_owned(),
            |dir| dir.to_string_lossy().into_owned(),
        )
    }

    /// Re-launches the agent of a dead session in its workspace, resuming
    /// the same conversation (native harness resume: `omp --resume=<path>`,
    /// `claude --resume <id>`, `codex resume <id>`), and re-binds the
    /// session to its post. `None` when the session is already being
    /// resumed by a concurrent message.
    pub async fn resume_session(
        &self,
        ctx: &Context,
        session: &SessionRow,
    ) -> BotResult<Option<Agent>> {
        let key = SessionPath::from(session.session_path.clone());
        {
            let mut resuming = self.resuming.lock().expect("resuming lock poisoned");
            if !resuming.insert(key.clone()) {
                return Ok(None);
            }
        }
        let result = self.resume_session_inner(ctx, session).await;
        self.resuming
            .lock()
            .expect("resuming lock poisoned")
            .remove(&key);
        result.map(Some)
    }

    async fn resume_session_inner(&self, ctx: &Context, session: &SessionRow) -> BotResult<Agent> {
        let post_id = session.post_channel_id.ok_or_else(|| {
            BotError::Other(format!(
                "session `{}` has no forum post",
                session.session_path
            ))
        })?;
        let post = from_i64(post_id)?;
        let kind = self
            .applied_kind(ctx, post)
            .await?
            .unwrap_or(crate::config::DEFAULT_AGENT_KIND);
        let args = resume_args(kind, session);
        let workspace = self.workspace_by_label(&session.workspace_label).await?;
        let title = self.forum_thread(ctx, post).await?.base.name;
        let base = sanitize_agent_name(&title).unwrap_or_else(|| "resumed".to_owned());
        let name = self.unique_agent_name(&base).await?;

        info!(
            session = %session.session_path,
            %name,
            kind = kind.as_str(),
            "resuming session in a new agent"
        );

        // The new workspace's label when the old one is gone: a fresh
        // workspace named after the agent.
        let workspace_label = workspace
            .as_ref()
            .map_or_else(|| name.clone(), |workspace| workspace.label.clone());
        let started = match workspace {
            Some(workspace) => {
                self.spawn_in_workspace(&workspace, &name, kind, &session.cwd, &args)
                    .await?
            }
            None => {
                self.spawn_in_new_workspace(&name, &name, kind, &session.cwd, &args)
                    .await?
            }
        };

        // The row follows the agent into its (possibly re-created)
        // workspace; the post binding, transcript, and sync cursor are
        // untouched — a native resume continues the same transcript.
        let updated = SessionRow {
            workspace_label,
            session_path: session.session_path.clone(),
            cwd: started.cwd.to_string_lossy().into_owned(),
            transcript_path: session.transcript_path.clone(),
            post_channel_id: session.post_channel_id,
            synced_messages: session.synced_messages,
            last_discord_message_id: session.last_discord_message_id,
            starter_message_id: session.starter_message_id,
        };
        self.db.upsert_session(&updated).await?;
        self.ensure_session_post(ctx, &started).await?;
        Ok(started)
    }

    /// Applies only the agent-kind tag to a dead session's post: the status
    /// tag is dropped; the thread itself is closed (archived — a message
    /// still unarchives it and resumes the session).
    async fn dead_post_tags(
        &self,
        ctx: &Context,
        forum: ChannelId,
        post: ChannelId,
    ) -> BotResult<()> {
        let ids = self.tag_ids(ctx, forum).await?;
        let kind_id = self
            .applied_kind(ctx, post)
            .await?
            .and_then(|kind| ids.get(kind.as_str()).copied());
        let applied = kind_id.into_iter().collect::<Vec<_>>();
        ThreadId::new(post.get())
            .edit(&ctx.http, EditThread::new().applied_tags(applied))
            .await?;
        Ok(())
    }

    /// Spawns a herdr agent in `workspace`: creates a tab and starts the
    /// agent in it under `name`. The tab is closed again if the agent fails
    /// to start.
    pub async fn spawn_in_workspace(
        &self,
        workspace: &Workspace,
        name: &str,
        kind: AgentKind,
        cwd: &str,
        args: &[String],
    ) -> BotResult<Agent> {
        let tab = match self
            .herdr
            .create_tab(&workspace.workspace_id, name, cwd)
            .await
        {
            Ok(tab) => tab,
            Err(error) => return Err(error.into()),
        };

        match self
            .herdr
            .start_agent(name, kind.as_str(), &tab.pane_id, args)
            .await
        {
            Ok(agent) => {
                info!(?agent, "started agent");
                Ok(agent)
            }
            Err(error) => {
                if let Err(close_error) = self.herdr.close_tab(&tab.tab_id).await {
                    warn!(
                        ?close_error,
                        "failed to clean up tab after failed agent start"
                    );
                }
                Err(error.into())
            }
        }
    }

    /// Spawns a herdr agent in a fresh workspace created with `label` and
    /// `cwd`, under `name`. The workspace is closed again if the agent
    /// fails to start.
    pub async fn spawn_in_new_workspace(
        &self,
        label: &str,
        name: &str,
        kind: AgentKind,
        cwd: &str,
        args: &[String],
    ) -> BotResult<Agent> {
        let created = self.herdr.create_workspace_with_pane(label, cwd).await?;

        match self
            .herdr
            .start_agent(name, kind.as_str(), &created.pane_id, args)
            .await
        {
            Ok(agent) => {
                info!(?agent, "started agent");
                Ok(agent)
            }
            Err(error) => {
                if let Err(close_error) = self
                    .herdr
                    .close_workspace(&created.workspace.workspace_id)
                    .await
                {
                    warn!(
                        ?close_error,
                        "failed to clean up workspace after failed agent start"
                    );
                }
                Err(error.into())
            }
        }
    }

    /// The session row for `agent`, when one exists.
    async fn session_for_agent(&self, agent: &Agent) -> Option<SessionRow> {
        let path = agent.agent_session.as_ref().map(|session| &session.value)?;
        self.db.get_session(path).await.ok().flatten()
    }

    /// The forum channel containing `post` (its parent channel).
    async fn forum_for_post(&self, ctx: &Context, post: ChannelId) -> BotResult<ChannelId> {
        match ctx.http.get_channel(post.widen()).await? {
            Channel::Guild(channel) => channel
                .parent_id
                .ok_or_else(|| BotError::Other(format!("post {post} is not in a forum"))),
            Channel::GuildThread(thread) => Ok(thread.parent_id),
            _ => Err(BotError::Other(format!(
                "post {post} is not a guild channel"
            ))),
        }
    }

    /// The herdr workspace with `workspace_id`, if any.
    async fn workspace_by_id(&self, workspace_id: &WorkspaceId) -> BotResult<Option<Workspace>> {
        let workspaces = self.herdr.list_workspaces().await?;
        Ok(workspaces
            .into_iter()
            .find(|workspace| workspace.workspace_id == *workspace_id))
    }

    /// The herdr workspace with `label`, if any.
    pub(crate) async fn workspace_by_label(&self, label: &str) -> BotResult<Option<Workspace>> {
        let workspaces = self.herdr.list_workspaces().await?;
        Ok(workspaces
            .into_iter()
            .find(|workspace| workspace.label == label))
    }

    /// The branch a worktree workspace has checked out, for the starter
    /// message's `worktree` field: `None` when the workspace is not a
    /// worktree, or its branch is unknown (e.g. detached).
    async fn worktree_branch(&self, workspace: &Workspace) -> Option<String> {
        workspace.worktree.as_ref()?;
        let list = self
            .herdr
            .worktree_list(&workspace.workspace_id)
            .await
            .ok()?;
        list.worktrees
            .into_iter()
            .find(|entry| {
                entry.open_workspace_id.as_deref() == Some(workspace.workspace_id.as_str())
            })
            .and_then(|entry| entry.branch)
    }

    /// The main (non-worktree) workspace of the repo `workspace_id` runs
    /// in, per `worktree.list`.
    async fn worktree_source(&self, workspace_id: &WorkspaceId) -> Option<WorkspaceId> {
        let list = self.herdr.worktree_list(workspace_id).await.ok()?;
        list.source
            .and_then(|source| source.source_workspace_id)
            .map(WorkspaceId::from)
    }

    /// The workspace whose forum `workspace` mirrors: a worktree resolves
    /// to its repo's main workspace when that is open, else the worktree
    /// itself (which then gets its own forum).
    async fn forum_workspace(&self, workspace: &Workspace) -> BotResult<Workspace> {
        if workspace.worktree.is_none() {
            return Ok(workspace.clone());
        }
        let Some(source) = self.worktree_source(&workspace.workspace_id).await else {
            return Ok(workspace.clone());
        };
        Ok(self
            .workspace_by_id(&source)
            .await?
            .unwrap_or_else(|| workspace.clone()))
    }

    /// Persists `workspace`'s forum mapping, creating the workspace row on
    /// first use.
    async fn upsert_forum(&self, workspace: &Workspace, forum_id: ChannelId) -> BotResult<()> {
        let forum_id = to_i64(forum_id)?;
        let row = match self.db.get_workspace(&workspace.label).await? {
            Some(mut row) => {
                row.forum_channel_id = Some(forum_id);
                row
            }
            None => WorkspaceRow {
                label: workspace.label.clone(),
                workspace_id: workspace.workspace_id.to_string(),
                forum_channel_id: Some(forum_id),
            },
        };
        self.db.upsert_workspace(&row).await?;
        Ok(())
    }

    async fn forum_channel(&self, ctx: &Context, channel_id: ChannelId) -> BotResult<GuildChannel> {
        match ctx.http.get_channel(channel_id.widen()).await? {
            Channel::Guild(channel) => Ok(channel),
            _ => Err(BotError::ForumChannelNotFound),
        }
    }

    /// The thread channel `thread_id` (a forum post) as a
    /// [`GuildThread`], whose parent is the forum channel.
    async fn forum_thread(&self, ctx: &Context, thread_id: ChannelId) -> BotResult<GuildThread> {
        match ctx.http.get_channel(thread_id.widen()).await? {
            Channel::GuildThread(thread) => Ok(thread),
            _ => Err(BotError::ForumChannelNotFound),
        }
    }
}

/// The unicode emoji of a forum tag, when it uses one.
fn tag_emoji(tag: &ForumTag) -> &str {
    match &tag.emoji {
        Some(ForumEmoji::Name(name)) => name,
        _ => "",
    }
}

/// Maps a forum's available tags by id, for resolving a thread's applied
/// tags to names.
fn tag_names(forum: &GuildChannel) -> HashMap<ForumTagId, &str> {
    forum
        .available_tags
        .iter()
        .map(|tag| (tag.id, tag.name.as_str()))
        .collect()
}

/// The `agent.start` arguments that resume `session`'s conversation in its
/// harness: omp resumes by transcript path, claude-code and codex by
/// session id (the row key — herdr's reported session reference), pi and
/// opencode by session id via `--session`.
#[must_use]
fn resume_args(kind: AgentKind, session: &SessionRow) -> Vec<String> {
    match kind {
        AgentKind::Omp => vec![format!("--resume={}", session.transcript_path)],
        AgentKind::ClaudeCode => vec!["--resume".into(), session.session_path.clone()],
        AgentKind::Codex => vec!["resume".into(), session.session_path.clone()],
        AgentKind::Pi | AgentKind::Opencode => {
            vec!["--session".into(), session.session_path.clone()]
        }
    }
}

/// Converts a Discord snowflake (channel or message id) to the database's
/// i64 representation.
fn to_i64(id: impl Into<u64>) -> BotResult<i64> {
    i64::try_from(id.into()).map_err(|_| BotError::Other("snowflake overflows i64".into()))
}

/// Converts a database i64 back into a Discord channel id.
pub fn from_i64(id: i64) -> BotResult<ChannelId> {
    u64::try_from(id)
        .map(ChannelId::new)
        .map_err(|_| BotError::Other(format!("{id} is not a valid channel id")))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        resume_args, sanitize_agent_name,
        titles::{forum_channel_name, post_title, session_intro},
    };
    use crate::{
        db::SessionRow,
        herdr::{Agent, PaneId, SessionPath, TabId, WorkspaceId},
        session::AgentKind,
    };

    #[test]
    fn sanitize_basic() {
        assert_eq!(
            sanitize_agent_name("My Cool Agent").as_deref(),
            Some("my-cool-agent")
        );
    }

    #[test]
    fn sanitize_collapses_separators() {
        assert_eq!(sanitize_agent_name("a--b___c").as_deref(), Some("a-b-c"));
    }

    #[test]
    fn sanitize_truncates() {
        assert_eq!(
            sanitize_agent_name(&"a".repeat(64)).as_deref(),
            Some("a".repeat(32).as_str())
        );
    }

    #[test]
    fn sanitize_prefixes_non_letter_start() {
        assert_eq!(
            sanitize_agent_name("123abc").as_deref(),
            Some("agent-123abc")
        );
        assert_eq!(sanitize_agent_name("🤖 bot").as_deref(), Some("bot"));
    }

    #[test]
    fn sanitize_rejects_unusable() {
        assert_eq!(sanitize_agent_name("💥💥"), None);
        assert_eq!(sanitize_agent_name("---"), None);
    }

    #[test]
    fn forum_channel_name_sanitizes() {
        assert_eq!(forum_channel_name("My Workspace"), "my-workspace");
        assert_eq!(forum_channel_name("UPPER  Case!!"), "upper-case");
        assert_eq!(forum_channel_name("💥"), "agents");
        let long = forum_channel_name(&"a".repeat(200));
        assert_eq!(long.len(), 100);
        assert!(long.ends_with('a'));
    }

    #[test]
    fn post_title_uses_stripped_terminal_title() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("  omp — my project   ".to_owned());
        assert_eq!(post_title(&agent, AgentKind::Omp), "omp — my project");
    }

    #[test]
    fn post_title_falls_back_to_kind_label() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("   ".to_owned());
        assert_eq!(post_title(&agent, AgentKind::Omp), "omp session");
        agent.terminal_title_stripped = None;
        assert_eq!(post_title(&agent, AgentKind::Codex), "codex session");
    }

    #[test]
    fn post_title_truncates() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("a".repeat(500));
        assert_eq!(post_title(&agent, AgentKind::Omp).chars().count(), 100);
    }

    #[test]
    fn resume_args_resume_by_kind() {
        let session = SessionRow {
            session_path: "s1".to_owned(),
            workspace_label: "w1".to_owned(),
            cwd: "/tmp".to_owned(),
            transcript_path: "/tmp/s1.jsonl".to_owned(),
            post_channel_id: Some(1),
            synced_messages: 0,
            last_discord_message_id: None,
            starter_message_id: None,
        };
        assert_eq!(
            resume_args(AgentKind::Omp, &session),
            vec!["--resume=/tmp/s1.jsonl"]
        );
        assert_eq!(
            resume_args(AgentKind::ClaudeCode, &session),
            vec!["--resume", "s1"]
        );
        assert_eq!(
            resume_args(AgentKind::Codex, &session),
            vec!["resume", "s1"]
        );
        assert_eq!(
            resume_args(AgentKind::Pi, &session),
            vec!["--session", "s1"]
        );
        assert_eq!(
            resume_args(AgentKind::Opencode, &session),
            vec!["--session", "s1"]
        );
    }

    #[test]
    fn session_intro_shows_live_pane() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(
                Some(&agent),
                None,
                Path::new("/home/me"),
                Some(&SessionPath::from("s1"))
            ),
            "`w1:p1` · cwd `/home/me` · session `s1`"
        );
    }

    #[test]
    fn session_intro_marks_inactive_and_skips_missing_session() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(None, None, Path::new("/home/me"), None),
            "inactive · cwd `/home/me`"
        );
        assert_eq!(
            session_intro(Some(&agent), None, Path::new("/home/me"), None),
            "`w1:p1` · cwd `/home/me`"
        );
    }

    #[test]
    fn session_intro_shows_worktree_after_pane() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(
                Some(&agent),
                Some("feature-x"),
                Path::new("/home/me"),
                Some(&SessionPath::from("s1"))
            ),
            "`w1:p1` · worktree `feature-x` · cwd `/home/me` · session `s1`"
        );
    }

    fn agent_fixture() -> Agent {
        Agent {
            agent: Some("omp".to_owned()),
            agent_status: "idle".to_owned(),
            name: Some("agent".to_owned()),
            pane_id: PaneId::from("w1:p1"),
            tab_id: TabId::from("w1:t1"),
            workspace_id: WorkspaceId::from("w1"),
            cwd: PathBuf::from("/home/me"),
            focused: false,
            launch_pending: false,
            terminal_title_stripped: None,
            agent_session: None,
        }
    }
}

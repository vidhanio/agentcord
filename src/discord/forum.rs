//! Discord forum primitives for session creation and import.

use std::{collections::HashMap, path::PathBuf};

use agent_client_protocol::schema::v1::SessionId;
use serenity::{
    all::{
        ChannelId, ChannelType, Context, CreateForumPost, CreateForumTag, CreateMessage,
        EditChannel, EditMessage, EditThread, ForumEmoji, ForumTag, ForumTagId, GenericChannelId,
        GetMessages, GuildChannel, GuildThread, MessageId, ReactionType, ThreadId,
        small_fixed_array::TruncatingInto,
    },
    http::{HttpError, JsonErrorCode},
};
use tracing::{debug, info, warn};

use super::shorten_home;
use crate::{
    Bot, BotError, BotResult,
    acp::default_model,
    config::{AgentKey, TagEmoji},
    db::SessionRow,
};

/// Discord's maximum forum post title length.
const FORUM_TITLE_LIMIT: usize = 100;

/// ACP metadata needed to create the Discord binding.
#[derive(Clone, Debug)]
pub struct SessionMetadata {
    /// Configured agent key used for the forum tag.
    pub agent_key: crate::config::AgentKey,
    /// Session working directory used in the forum title.
    pub project_path: std::path::PathBuf,
    /// Agent-owned ACP session identifier.
    pub session_id: SessionId,
    /// Optional agent-provided title, used in the forum title.
    pub title: Option<String>,
    /// Current ACP model shown in the starter message; not persisted locally.
    pub model: Option<String>,
}

impl SessionMetadata {
    /// Builds display metadata from a persisted session binding.
    fn from_row(row: &SessionRow, model: Option<&str>) -> Self {
        Self {
            agent_key: row.agent_key.clone(),
            project_path: row.project_path.clone(),
            session_id: row.session_id.clone(),
            title: None,
            model: model.map(str::to_owned),
        }
    }

    /// Builds a bounded forum title from this session's metadata.
    fn post_title(&self) -> String {
        let fallback = format!("session {}", self.session_id);
        let title = self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&fallback);
        let project_path = shorten_home(&self.project_path.display().to_string());
        let raw = format!("{project_path} · {title}")
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        truncate_end(raw.trim(), FORUM_TITLE_LIMIT)
    }

    /// Renders the immutable starter message for this session post.
    fn starter_message(&self) -> String {
        let model = self
            .model
            .as_deref()
            .map(|model| format!(" · model `{}`", escape_inline(model)))
            .unwrap_or_default();
        format!(
            "session `{}`{model}",
            escape_inline(&self.session_id.to_string())
        )
    }
}

impl Bot {
    /// Updates the first session message with the current ACP model.
    pub(crate) async fn update_session_starter(
        &self,
        row: &SessionRow,
        model: Option<&str>,
    ) -> BotResult {
        let context = self.context()?.clone();
        let metadata = SessionMetadata::from_row(row, model);
        let starter_content = metadata.starter_message();
        debug!(thread = ?row.thread_id, "reading session starter message...");
        let messages = row
            .thread_id
            .messages(
                &context.http,
                GetMessages::new().limit(1).after(MessageId::new(1)),
            )
            .await?;
        let Some(starter) = messages.first() else {
            warn!(thread = ?row.thread_id, "session starter message is missing");
            return Ok(());
        };
        if starter.content == starter_content {
            debug!(thread = ?row.thread_id, "session starter message is already current");
            return Ok(());
        }
        debug!(thread = ?row.thread_id, message = ?starter.id, "editing session starter message...");
        row.thread_id
            .edit_message(
                &context.http,
                starter.id,
                EditMessage::new().content(starter_content),
            )
            .await?;
        debug!(thread = ?row.thread_id, message = ?starter.id, "edited session starter message");
        Ok(())
    }

    /// Creates an ACP session and binds it to a new forum post.
    pub async fn create_session(
        &self,
        agent_key: &AgentKey,
        project_path: PathBuf,
    ) -> BotResult<GenericChannelId> {
        info!(
            agent = %agent_key,
            project = ?project_path,
            "creating session binding..."
        );
        let created = self
            .state()
            .supervisor
            .new_session(self, agent_key, project_path.clone())
            .await?;
        debug!(
            agent = %agent_key,
            session = %created.session_id,
            options = created.config_options.len(),
            "received new acp session"
        );
        let model = default_model(&created.config_options);
        let metadata = SessionMetadata {
            agent_key: agent_key.clone(),
            project_path,
            session_id: created.session_id.clone(),
            title: None,
            model,
        };
        let row = self.create_and_store_session(&metadata).await?;
        self.state().supervisor.start_new(self, &row, created);
        info!(thread = ?row.thread_id, "session is ready");
        Ok(row.thread_id)
    }

    /// Imports an ACP session exposed by `session/list` into a forum post.
    pub async fn import_session(
        &self,
        agent_key: &AgentKey,
        session_id: &SessionId,
    ) -> BotResult<GenericChannelId> {
        info!(
            agent = %agent_key,
            session = %session_id,
            "importing session binding..."
        );
        if session_id.to_string().trim().is_empty() {
            return Err(BotError::EmptySessionId);
        }
        if let Some(existing) = self.db().session_by_agent(agent_key, session_id).await? {
            if self.session_thread_exists(existing.thread_id).await? {
                return Err(BotError::AlreadyImported {
                    thread: existing.thread_id,
                });
            }
            info!(
                thread = ?existing.thread_id,
                session = %session_id,
                "removing stale session binding..."
            );
            self.state()
                .supervisor
                .stop_and_wait(self, existing.thread_id)
                .await?;
            self.db().delete_session(existing.thread_id).await?;
            info!(
                thread = ?existing.thread_id,
                "removed stale session binding after thread deletion"
            );
        }
        let imported = self
            .state()
            .supervisor
            .inspect_session(self, agent_key, session_id)
            .await?;
        if !imported.project_path.is_absolute() {
            return Err(BotError::NonAbsoluteProjectPath {
                path: imported.project_path,
            });
        }
        let project_path = imported
            .project_path
            .canonicalize()
            .unwrap_or(imported.project_path);
        self.state()
            .supervisor
            .validate_session(self, agent_key, &imported.session_id, &project_path)
            .await?;
        debug!(
            agent = %agent_key,
            session = %imported.session_id,
            project = ?project_path,
            "validated imported session"
        );
        let metadata = SessionMetadata {
            agent_key: agent_key.clone(),
            project_path,
            session_id: imported.session_id,
            title: imported.title,
            model: None,
        };
        let row = self.create_and_store_session(&metadata).await?;
        self.state().supervisor.start(self, &row, Vec::new());
        info!(thread = ?row.thread_id, "imported session is ready");
        Ok(row.thread_id)
    }

    /// Creates a forum post and stores its durable session binding.
    async fn create_and_store_session(&self, metadata: &SessionMetadata) -> BotResult<SessionRow> {
        let row = self.create_session_post(metadata).await?;
        if let Err(error) = self.db().insert_session(&row).await {
            warn!(?error, thread = ?row.thread_id, "failed to store session binding");
            let context = self.context()?.clone();
            match row.thread_id.delete(&context.http, None).await {
                Ok(_) => debug!(
                    thread = ?row.thread_id,
                    "deleted session post after database error"
                ),
                Err(cleanup_error) => warn!(
                    ?cleanup_error,
                    thread = ?row.thread_id,
                    "failed to delete session post after database error"
                ),
            }
            return Err(error);
        }
        Ok(row)
    }

    /// Creates and tags a forum post for a newly bound ACP session.
    pub(crate) async fn create_session_post(
        &self,
        metadata: &SessionMetadata,
    ) -> BotResult<SessionRow> {
        let context = self.context()?.clone();
        let tags = self.tag_ids(&context).await?;
        let tag =
            tags.get(&metadata.agent_key)
                .copied()
                .ok_or_else(|| BotError::MissingForumTag {
                    agent_key: metadata.agent_key.to_string(),
                })?;
        let title = metadata.post_title();
        info!(
            agent = %metadata.agent_key,
            session = %metadata.session_id,
            "creating session forum post..."
        );
        let created = match self
            .config()
            .discord
            .forum_channel_id
            .create_forum_post(
                &context.http,
                CreateForumPost::new(
                    title,
                    CreateMessage::new().content(metadata.starter_message()),
                ),
            )
            .await
        {
            Ok(created) => created,
            Err(error) => {
                warn!(
                    ?error,
                    agent = %metadata.agent_key,
                    session = %metadata.session_id,
                    "failed to create session forum post"
                );
                return Err(error.into());
            }
        };
        info!(
            agent = %metadata.agent_key,
            session = %metadata.session_id,
            thread = ?created.id,
            "created session forum post"
        );
        debug!(thread = ?created.id, "applying session forum tag...");
        if let Err(error) = ThreadId::new(created.id.get())
            .edit(&context.http, EditThread::new().applied_tags(vec![tag]))
            .await
        {
            warn!(
                ?error,
                thread = %created.id,
                "failed to apply session forum tag"
            );
            warn!(
                thread = %created.id,
                "deleting untagged forum post..."
            );
            match created.id.widen().delete(&context.http, None).await {
                Ok(_) => debug!(thread = %created.id, "deleted untagged forum post"),
                Err(cleanup_error) => {
                    warn!(
                        ?cleanup_error,
                        thread = %created.id,
                        "failed to delete untagged forum post"
                    );
                }
            }
            return Err(error.into());
        }
        debug!(thread = ?created.id, "applied session forum tag");

        Ok(SessionRow {
            thread_id: created.id.widen(),
            agent_key: metadata.agent_key.clone(),
            session_id: metadata.session_id.clone(),
            project_path: metadata.project_path.clone(),
        })
    }

    /// Checks that the configured forum is available and has agent tags.
    pub(crate) async fn validate_and_reconcile_forum(&self) -> BotResult {
        info!("validating configured forum...");
        let context = self.context()?.clone();
        let tag_ids = self.tag_ids(&context).await?;
        let result = self.reconcile_threads(&context, &tag_ids).await;
        if result.is_ok() {
            info!("validated configured forum");
        }
        result
    }

    /// Fetches the configured forum and ensures every agent has a tag.
    async fn tag_ids(
        &self,
        context: &Context,
    ) -> BotResult<HashMap<crate::config::AgentKey, ForumTagId>> {
        debug!("fetching configured forum tags...");
        let mut channel = self.forum_channel(context).await?;
        let desired = self
            .config()
            .agents
            .iter()
            .map(|(key, agent)| {
                channel
                    .available_tags
                    .iter()
                    .find(|tag| {
                        tag.name == key.as_ref()
                            && emoji_key(tag.emoji.as_ref()) == configured_emoji_key(&agent.emoji)
                    })
                    .map_or_else(
                        || CreateForumTag::new(key.as_ref()).emoji(reaction(&agent.emoji)),
                        copy_tag,
                    )
            })
            .collect::<Vec<_>>();
        let current = channel
            .available_tags
            .iter()
            .map(|tag| (tag.name.to_string(), emoji_key(tag.emoji.as_ref())))
            .collect::<Vec<_>>();
        let wanted = self
            .config()
            .agents
            .iter()
            .map(|(key, agent)| (key.to_string(), configured_emoji_key(&agent.emoji)))
            .collect::<Vec<_>>();
        if current.len() != wanted.len() || wanted.iter().any(|tag| !current.contains(tag)) {
            info!(
                current = current.len(),
                configured = wanted.len(),
                "updating forum tags..."
            );
            if let Err(error) = channel
                .id
                .edit(&context.http, EditChannel::new().available_tags(desired))
                .await
            {
                warn!(
                    ?error,
                    forum = %channel.id,
                    "failed to update forum tags"
                );
                return Err(error.into());
            }
            info!("updated forum tags");
            debug!("refetching configured forum tags...");
            channel = self.forum_channel(context).await?;
        }

        let tags = self
            .config()
            .agents
            .keys()
            .filter_map(|key| {
                channel
                    .available_tags
                    .iter()
                    .find(|tag| tag.name == key.as_ref())
                    .map(|tag| (key.clone(), tag.id))
            })
            .collect::<HashMap<_, _>>();
        debug!(available = tags.len(), "resolved configured forum tags");
        Ok(tags)
    }

    /// Fetches all active and archived threads belonging to the configured
    /// forum.
    async fn forum_threads(
        &self,
        context: &Context,
        forum: ChannelId,
    ) -> BotResult<HashMap<GenericChannelId, GuildThread>> {
        let mut threads = self.active_forum_threads(context, forum).await?;
        threads.extend(self.archived_forum_threads(context, forum).await?);
        Ok(threads)
    }

    /// Fetches active threads belonging to the configured forum.
    async fn active_forum_threads(
        &self,
        context: &Context,
        forum: ChannelId,
    ) -> BotResult<HashMap<GenericChannelId, GuildThread>> {
        debug!(
            guild = %self.config().discord.guild_id,
            forum = %forum,
            "listing active forum threads..."
        );
        let active = match self
            .config()
            .discord
            .guild_id
            .get_active_threads(&context.http)
            .await
        {
            Ok(active) => active,
            Err(error) => {
                warn!(
                    ?error,
                    guild = %self.config().discord.guild_id,
                    forum = %forum,
                    "failed to list active forum threads"
                );
                return Err(error.into());
            }
        };
        let received = active.threads.len();
        let mut threads = HashMap::new();
        let mut forum_threads = 0;
        for thread in active
            .threads
            .into_iter()
            .filter(|thread| thread.parent_id == forum)
        {
            forum_threads += 1;
            threads.insert(thread.id.widen(), thread);
        }
        debug!(received, forum_threads, "listed active forum threads");
        Ok(threads)
    }

    /// Fetches archived public threads belonging to the configured forum.
    async fn archived_forum_threads(
        &self,
        context: &Context,
        forum: ChannelId,
    ) -> BotResult<HashMap<GenericChannelId, GuildThread>> {
        let mut threads = HashMap::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            debug!(
                forum = %forum,
                page = pages + 1,
                before = ?before,
                "listing archived forum threads..."
            );
            let page = match forum
                .get_archived_public_threads(&context.http, before, Some(100))
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    warn!(
                        ?error,
                        forum = %forum,
                        page = pages + 1,
                        "failed to list archived forum threads"
                    );
                    return Err(error.into());
                }
            };
            pages += 1;
            let received = page.threads.len();
            let forum_threads = page
                .threads
                .iter()
                .filter(|thread| thread.parent_id == forum)
                .count();
            let has_more = page.has_more;
            let next_before = page.threads.iter().last().map(|thread| {
                thread
                    .thread_metadata
                    .archive_timestamp
                    .unwrap_or_else(|| thread.id.created_at())
            });
            for thread in page
                .threads
                .into_iter()
                .filter(|thread| thread.parent_id == forum)
            {
                threads.insert(thread.id.widen(), thread);
            }
            debug!(
                pages,
                received, forum_threads, has_more, "listed archived forum threads"
            );
            if !has_more {
                break;
            }
            let Some(next_before) = next_before else {
                warn!(
                    forum = %forum,
                    page = pages,
                    "archived forum thread pagination returned no cursor"
                );
                break;
            };
            if before == Some(next_before) {
                warn!(
                    forum = %forum,
                    page = pages,
                    "archived forum thread pagination did not advance"
                );
                break;
            }
            before = Some(next_before);
        }
        Ok(threads)
    }

    /// Reconciles active managed threads and deletes active unmanaged threads.
    async fn reconcile_threads(
        &self,
        context: &Context,
        tag_ids: &HashMap<crate::config::AgentKey, ForumTagId>,
    ) -> BotResult {
        let managed = self
            .db()
            .sessions()
            .await?
            .into_iter()
            .map(|session| (session.thread_id, session))
            .collect::<HashMap<_, _>>();
        debug!(managed = managed.len(), "loaded managed forum sessions");
        let forum = self.config().discord.forum_channel_id;
        let threads = self.forum_threads(context, forum).await?;
        let discovered = threads.len();
        let mut deleted = 0;
        let mut retagged = 0;
        let mut failed = 0;
        let mut reconcile_error = None;
        for (thread_id, thread) in threads {
            if thread.thread_metadata.archived() {
                debug!(thread = ?thread_id, "leaving archived forum thread unmanaged");
                continue;
            }
            let Some(row) = managed.get(&thread_id) else {
                info!(thread = ?thread_id, "deleting unmanaged forum thread...");
                match thread_id.delete(&context.http, None).await {
                    Ok(_) => {
                        deleted += 1;
                        info!(thread = ?thread_id, "deleted unmanaged forum thread");
                    }
                    Err(error) => {
                        failed += 1;
                        warn!(?error, thread = ?thread_id, "failed to delete unmanaged forum thread");
                        if reconcile_error.is_none() {
                            reconcile_error = Some(error.into());
                        }
                    }
                }
                continue;
            };
            let Some(tag) = tag_ids.get(&row.agent_key).copied() else {
                let error = BotError::MissingForumTag {
                    agent_key: row.agent_key.to_string(),
                };
                failed += 1;
                warn!(?error, thread = ?thread_id, "failed to find session forum tag");
                if reconcile_error.is_none() {
                    reconcile_error = Some(error);
                }
                continue;
            };
            if let Err(error) = self.reconcile_thread(context, &thread, row, tag).await {
                failed += 1;
                warn!(?error, thread = ?thread_id, "failed to reconcile managed forum thread");
                if reconcile_error.is_none() {
                    reconcile_error = Some(error);
                }
            } else {
                retagged += 1;
                debug!(thread = ?thread_id, agent = %row.agent_key, "reconciled managed forum thread");
            }
        }
        info!(
            managed = managed.len(),
            discovered, deleted, retagged, failed, "reconciled forum threads"
        );
        reconcile_error.map_or(Ok(()), Err)
    }

    /// Stops managed sessions when archived and reconciles them when reopened.
    pub(crate) async fn handle_forum_thread_update(
        &self,
        old: Option<&GuildThread>,
        thread: &GuildThread,
    ) -> BotResult {
        if thread.parent_id != self.config().discord.forum_channel_id {
            return Ok(());
        }
        let is_archived = thread.thread_metadata.archived();
        if is_archived {
            if old.is_some_and(|old| old.thread_metadata.archived()) {
                return Ok(());
            }
        } else if old.is_some_and(|old| !old.thread_metadata.archived()) {
            return Ok(());
        }
        let thread_id = thread.id.widen();
        let Some(row) = self.db().session(thread_id).await? else {
            return Ok(());
        };
        if is_archived {
            info!(thread = ?thread_id, "stopping archived managed session...");
            self.state()
                .supervisor
                .stop_and_wait(self, thread_id)
                .await?;
            info!(thread = ?thread_id, "stopped archived managed session");
            return Ok(());
        }

        let context = self.context()?.clone();
        let tag_ids = self.tag_ids(&context).await?;
        let tag =
            tag_ids
                .get(&row.agent_key)
                .copied()
                .ok_or_else(|| BotError::MissingForumTag {
                    agent_key: row.agent_key.to_string(),
                })?;
        self.reconcile_thread(&context, thread, &row, tag).await?;
        self.state().supervisor.start(self, &row, Vec::new());
        info!(thread = ?thread_id, "reconciled and loaded unarchived managed session");
        Ok(())
    }

    /// Updates a managed thread's title and starter message along with its
    /// configured tag.
    async fn reconcile_thread(
        &self,
        context: &Context,
        thread: &GuildThread,
        row: &SessionRow,
        tag: ForumTagId,
    ) -> BotResult {
        let metadata = SessionMetadata::from_row(row, None);
        self.retag_thread(context, thread, row, tag, &metadata.post_title())
            .await
    }

    /// Applies the configured title and tag to an active thread.
    async fn retag_thread(
        &self,
        context: &Context,
        thread: &GuildThread,
        row: &SessionRow,
        tag: ForumTagId,
        title: &str,
    ) -> BotResult {
        let update = EditThread::new().name(title).applied_tags(vec![tag]);
        if thread.thread_metadata.archived() {
            return Ok(());
        }
        debug!(
            thread = %thread.id,
            tag = %tag,
            "updating managed forum thread title and tag..."
        );
        thread.id.edit(&context.http, update).await?;
        debug!(thread = %thread.id, "updated managed forum thread title and tag");
        self.update_session_starter(row, None).await
    }

    /// Fetches and validates the configured Discord forum channel.
    async fn forum_channel(&self, context: &Context) -> BotResult<GuildChannel> {
        let forum = self.config().discord.forum_channel_id;
        let guild = self.config().discord.guild_id;
        debug!(
            forum = %forum,
            guild = %guild,
            "fetching configured forum channel..."
        );
        let channel = match forum.to_guild_channel(context, Some(guild)).await {
            Ok(channel) => channel,
            Err(error) => {
                warn!(
                    ?error,
                    forum = %forum,
                    guild = %guild,
                    "failed to fetch configured forum channel"
                );
                return Err(error.into());
            }
        };
        if channel.base.kind == ChannelType::Forum {
            debug!(
                forum = %forum,
                tags = channel.available_tags.len(),
                "fetched configured forum channel"
            );
            Ok(channel)
        } else {
            warn!(
                forum = %forum,
                kind = ?channel.base.kind,
                "configured channel is not a forum"
            );
            Err(BotError::ForumChannelRequired {
                channel: forum.to_string(),
            })
        }
    }

    /// Checks whether a persisted session thread still exists in Discord.
    async fn session_thread_exists(&self, thread: GenericChannelId) -> BotResult<bool> {
        let context = self.context()?.clone();
        debug!(thread = ?thread, "checking whether session thread exists...");
        match context.http.get_channel(thread).await {
            Ok(_) => {
                debug!(thread = ?thread, "session thread exists");
                Ok(true)
            }
            Err(error) if is_unknown_channel(&error) => {
                debug!(thread = ?thread, "session thread does not exist");
                Ok(false)
            }
            Err(error) => {
                warn!(
                    ?error,
                    thread = ?thread,
                    "failed to check whether session thread exists"
                );
                Err(error.into())
            }
        }
    }
}

/// Identifies Discord's response for a missing channel or thread.
fn is_unknown_channel(error: &serenity::Error) -> bool {
    matches!(
        error,
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
            if response.error.code == JsonErrorCode::UnknownChannel
    )
}

/// Copies an existing forum tag while preserving its moderation and emoji.
fn copy_tag(tag: &ForumTag) -> CreateForumTag<'_> {
    let mut created = CreateForumTag::new(&tag.name).moderated(tag.moderated);
    if let Some(emoji) = &tag.emoji {
        let reaction = match emoji {
            ForumEmoji::Id(id) => Some(ReactionType::Custom {
                animated: false,
                id: *id,
                name: None,
            }),
            ForumEmoji::Name(name) => Some(ReactionType::Unicode(name.to_string().trunc_into())),
            _ => None,
        };
        if let Some(reaction) = reaction {
            created = created.emoji(reaction);
        }
    }
    created
}

/// Converts configured tag emoji data into a Discord reaction.
fn reaction(emoji: &TagEmoji) -> ReactionType {
    match emoji {
        TagEmoji::Unicode(value) => ReactionType::Unicode(value.clone().trunc_into()),
        TagEmoji::Custom { id, animated } => ReactionType::Custom {
            animated: *animated,
            id: *id,
            name: None,
        },
    }
}

/// Produces a comparable key for an existing Discord forum emoji.
fn emoji_key(emoji: Option<&ForumEmoji>) -> String {
    match emoji {
        Some(ForumEmoji::Id(id)) => format!("id:{}", id.get()),
        Some(ForumEmoji::Name(name)) => format!("unicode:{name}"),
        None => "none".into(),
        _ => "unknown".into(),
    }
}

/// Produces a comparable key for a configured forum emoji.
fn configured_emoji_key(emoji: &TagEmoji) -> String {
    match emoji {
        TagEmoji::Unicode(value) => format!("unicode:{value}"),
        TagEmoji::Custom { id, .. } => format!("id:{id}"),
    }
}

/// Escapes values embedded in inline Markdown code spans.
fn escape_inline(value: &str) -> String {
    value.replace('`', "ˋ").replace(['\n', '\r'], " ")
}

/// Truncates without splitting a Unicode scalar value.
fn truncate_end(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

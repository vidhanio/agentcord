//! Discord forum primitives for session creation and import.

use std::{collections::HashMap, path::PathBuf};

use agent_client_protocol::schema::v1::SessionId;
use serenity::{
    all::{
        ChannelType, Context, CreateForumPost, CreateForumTag, CreateMessage, EditChannel,
        EditThread, ForumEmoji, ForumTag, ForumTagId, GenericChannelId, GuildChannel, ReactionType,
        ThreadId, small_fixed_array::TruncatingInto,
    },
    http::{HttpError, JsonErrorCode},
};
use tracing::{info, warn};

use crate::{
    Bot, BotError, BotResult,
    acp::default_model,
    config::{AgentKey, TagEmoji},
    db::SessionRow,
};

/// ACP metadata needed to create the Discord binding.
#[derive(Clone, Debug)]
pub struct SessionMetadata {
    /// Configured agent key used for the forum tag.
    pub agent_key: crate::config::AgentKey,
    /// Session working directory shown in the starter message.
    pub project_path: std::path::PathBuf,
    /// Agent-owned ACP session identifier.
    pub session_id: SessionId,
    /// Optional agent-provided title, used in the forum title.
    pub title: Option<String>,
    /// Current ACP model shown in the starter message; not persisted locally.
    pub model: Option<String>,
}

impl SessionMetadata {
    /// Builds a bounded forum title from this session's metadata.
    fn post_title(&self) -> String {
        let fallback = format!(
            "session {}",
            self.session_id
                .to_string()
                .chars()
                .take(12)
                .collect::<String>()
        );
        let title = self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&fallback);
        let raw = format!("{} · {title}", self.project_path.display())
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        truncate_end(raw.trim(), 100)
    }

    /// Renders the immutable starter message for this session post.
    fn starter_message(&self) -> String {
        let model = self
            .model
            .as_deref()
            .map(|model| format!(" · model `{}`", escape_inline(model)))
            .unwrap_or_default();
        format!(
            "session `{}` · cwd `{}`{model}",
            escape_inline(&self.session_id.to_string()),
            escape_inline(&self.project_path.display().to_string())
        )
    }
}

impl Bot {
    /// Creates an ACP session and binds it to a new forum post.
    pub async fn create_session(
        &self,
        agent_key: &AgentKey,
        project_path: PathBuf,
    ) -> BotResult<GenericChannelId> {
        let created = self
            .state()
            .supervisor
            .new_session(self, agent_key, project_path.clone())
            .await?;
        let model = default_model(&created.config_options);
        let metadata = SessionMetadata {
            agent_key: agent_key.clone(),
            project_path,
            session_id: created.session_id.clone(),
            title: None,
            model,
        };
        let row = self.create_session_post(&metadata).await?;
        if let Err(error) = self.db().insert_session(&row).await {
            let context = self.context()?.clone();
            if let Err(cleanup_error) = row.thread_id.delete(&context.http, None).await {
                warn!(
                    ?cleanup_error,
                    thread = ?row.thread_id,
                    "failed to delete session post after database error"
                );
            }
            return Err(error);
        }
        self.state().supervisor.start_new(self, &row, created);
        Ok(row.thread_id)
    }

    /// Imports an ACP session exposed by `session/list` into a forum post.
    pub async fn import_session(
        &self,
        agent_key: &AgentKey,
        session_id: &SessionId,
    ) -> BotResult<GenericChannelId> {
        if session_id.to_string().trim().is_empty() {
            return Err(BotError::EmptySessionId);
        }
        if let Some(existing) = self.db().session_by_agent(agent_key, session_id).await? {
            if self.session_thread_exists(existing.thread_id).await? {
                return Err(BotError::AlreadyImported {
                    thread: existing.thread_id,
                });
            }
            self.state().supervisor.stop(existing.thread_id);
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
        let metadata = SessionMetadata {
            agent_key: agent_key.clone(),
            project_path,
            session_id: imported.session_id,
            title: imported.title,
            model: None,
        };
        let row = self.create_session_post(&metadata).await?;
        if let Err(error) = self.db().insert_session(&row).await {
            let context = self.context()?.clone();
            if let Err(cleanup_error) = row.thread_id.delete(&context.http, None).await {
                warn!(
                    ?cleanup_error,
                    thread = ?row.thread_id,
                    "failed to delete imported session post after database error"
                );
            }
            return Err(error);
        }
        self.state().supervisor.start(self, &row, Vec::new());
        Ok(row.thread_id)
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
        let created = self
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
            .await?;
        if let Err(error) = ThreadId::new(created.id.get())
            .edit(&context.http, EditThread::new().applied_tags(vec![tag]))
            .await
        {
            if let Err(cleanup_error) = created.id.widen().delete(&context.http, None).await {
                warn!(
                    ?cleanup_error,
                    thread = %created.id,
                    "failed to delete untagged forum post"
                );
            }
            return Err(error.into());
        }

        Ok(SessionRow {
            thread_id: created.id.widen(),
            agent_key: metadata.agent_key.clone(),
            session_id: metadata.session_id.clone(),
            project_path: metadata.project_path.clone(),
        })
    }

    /// Checks that the configured forum is available and has agent tags.
    pub(crate) async fn validate_and_reconcile_forum(&self) -> BotResult {
        let context = self.context()?.clone();
        self.tag_ids(&context).await.map(|_| ())
    }

    /// Fetches the configured forum and ensures every agent has a tag.
    async fn tag_ids(
        &self,
        context: &Context,
    ) -> BotResult<HashMap<crate::config::AgentKey, ForumTagId>> {
        let mut channel = self.forum_channel(context).await?;
        let configured_names = self
            .config()
            .agents
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mut desired = channel
            .available_tags
            .iter()
            .filter(|tag| !configured_names.contains(&tag.name.to_string()))
            .map(copy_tag)
            .collect::<Vec<_>>();
        desired.extend(
            self.config().agents.iter().map(|(key, agent)| {
                CreateForumTag::new(key.as_ref()).emoji(reaction(&agent.emoji))
            }),
        );
        if desired.len() > 20 {
            return Err(BotError::TooManyForumTags {
                configured: self.config().agents.len(),
                limit: 20,
            });
        }

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
        if wanted.iter().any(|tag| !current.contains(tag)) {
            channel
                .id
                .edit(&context.http, EditChannel::new().available_tags(desired))
                .await?;
            channel = self.forum_channel(context).await?;
        }

        Ok(self
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
            .collect())
    }

    /// Fetches and validates the configured Discord forum channel.
    async fn forum_channel(&self, context: &Context) -> BotResult<GuildChannel> {
        let channel = self
            .config()
            .discord
            .forum_channel_id
            .to_guild_channel(context, Some(self.config().discord.guild_id))
            .await?;
        if channel.base.kind == ChannelType::Forum {
            Ok(channel)
        } else {
            Err(BotError::ForumChannelRequired {
                channel: self.config().discord.forum_channel_id.to_string(),
            })
        }
    }

    /// Checks whether a persisted session thread still exists in Discord.
    async fn session_thread_exists(&self, thread: GenericChannelId) -> BotResult<bool> {
        let context = self.context()?.clone();
        match context.http.get_channel(thread).await {
            Ok(_) => Ok(true),
            Err(error) if is_unknown_channel(&error) => Ok(false),
            Err(error) => Err(error.into()),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_client_protocol::schema::v1::SessionId;

    use super::SessionMetadata;
    use crate::config::AgentKey;

    /// Verifies that a selected model is included in the starter message.
    #[test]
    fn starter_message_includes_model() {
        let metadata = SessionMetadata {
            agent_key: AgentKey::new("example"),
            project_path: PathBuf::from("/work/project"),
            session_id: SessionId::new("session-1"),
            title: None,
            model: Some("openai/gpt-4o:high".into()),
        };
        assert_eq!(
            metadata.starter_message(),
            "session `session-1` · cwd `/work/project` · model `openai/gpt-4o:high`"
        );
    }
}

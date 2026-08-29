//! Discord forum primitives for session creation and import.

use std::collections::HashMap;

use agent_client_protocol::schema::v1::SessionId;
use serenity::all::{
    ChannelType, Context, CreateForumPost, CreateForumTag, CreateMessage, EditChannel, EditThread,
    ForumEmoji, ForumTag, ForumTagId, GuildChannel, ReactionType, ThreadId,
    small_fixed_array::TruncatingInto,
};

use crate::{Bot, BotError, BotResult, config::TagEmoji, db::SessionRow};

/// ACP metadata needed to create the Discord binding.
#[derive(Clone, Debug)]
pub struct SessionMetadata {
    /// Configured agent key used for the forum tag.
    pub agent_key: crate::config::AgentKey,
    /// Short project label used in the forum title.
    pub project_label: String,
    /// Session working directory shown in the starter message.
    pub project_path: std::path::PathBuf,
    /// Agent-owned ACP session identifier.
    pub session_id: SessionId,
    /// Optional agent-provided title, used in the forum title.
    pub title: Option<String>,
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
        let raw = format!("{} · {title}", self.project_label)
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
        format!(
            "session `{}` · cwd `{}`",
            escape_inline(&self.session_id.to_string()),
            escape_inline(&self.project_path.display().to_string())
        )
    }
}

impl Bot {
    /// Creates and tags a forum post for a newly bound ACP session.
    pub(crate) async fn create_session_post(
        &self,
        metadata: &SessionMetadata,
    ) -> BotResult<SessionRow> {
        let context = self.context()?.clone();
        let tags = self.tag_ids(&context).await?;
        let tag = tags.get(&metadata.agent_key).copied().ok_or_else(|| {
            BotError::Config(format!("missing forum tag for `{}`", metadata.agent_key))
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
            let _ = created.id.widen().delete(&context.http, None).await;
            return Err(error.into());
        }

        Ok(SessionRow {
            thread_id: created.id.widen(),
            agent_key: metadata.agent_key.clone(),
            session_id: metadata.session_id.clone(),
            project_path: metadata.project_path.clone(),
        })
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
            return Err(BotError::Config(format!(
                "the forum's existing tags plus {} configured agent tags exceed Discord's 20-tag limit",
                self.config().agents.len()
            )));
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
            Err(BotError::Config(format!(
                "Discord channel {} is not a forum",
                self.config().discord.forum_channel_id
            )))
        }
    }
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

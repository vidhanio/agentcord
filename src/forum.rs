use std::{collections::HashMap, fmt::Write};

use agent_client_protocol::schema::v1::UsageUpdate;
use serenity::all::{
    Channel, ChannelType, Context, CreateForumPost, CreateForumTag, CreateMessage, EditChannel,
    EditMessage, EditThread, EmojiId, ForumEmoji, ForumTag, ForumTagId, GenericChannelId,
    GetMessages, GuildChannel, GuildThread, MessageId, ReactionType, ThreadId,
    small_fixed_array::TruncatingInto,
};

use crate::{
    Bot, BotError, BotResult,
    config::TagEmoji,
    db::{Availability, SessionRow},
};

const THREAD_TITLE_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct SessionMetadata {
    pub agent_key: String,
    pub project_label: String,
    pub cwd: String,
    pub session_id: String,
    pub protocol_version: String,
    pub capabilities_json: String,
    pub restorable: bool,
    pub title: Option<String>,
}

impl Bot {
    pub async fn validate_and_reconcile_forum(&self) -> BotResult {
        let ctx = self.context()?;
        self.channel(ctx).await?;
        self.tag_ids(ctx).await?;
        Ok(())
    }

    pub async fn create_session_post(&self, metadata: &SessionMetadata) -> BotResult<SessionRow> {
        let ctx = self.context()?;
        let tags = self.tag_ids(ctx).await?;
        self.config
            .agents
            .get(&metadata.agent_key)
            .ok_or_else(|| BotError::Config(format!("unknown agent `{}`", metadata.agent_key)))?;
        let tag = tags.get(&metadata.agent_key).copied().ok_or_else(|| {
            BotError::Other(format!("missing forum tag for `{}`", metadata.agent_key))
        })?;
        let title = post_title(
            &metadata.project_label,
            metadata.title.as_deref(),
            &metadata.session_id,
        );
        let created = self
            .config
            .discord
            .forum_channel_id
            .create_forum_post(
                &ctx.http,
                CreateForumPost::new(
                    &title,
                    CreateMessage::new().content(starter_message(metadata, None)),
                ),
            )
            .await?;
        let starter_message_id = created
            .base
            .last_message_id
            .ok_or_else(|| BotError::Other("new forum post has no starter message".into()))?;
        ThreadId::new(created.id.get())
            .edit(&ctx.http, EditThread::new().applied_tags(vec![tag]))
            .await?;

        Ok(SessionRow {
            thread_id: created.id.widen(),
            starter_message_id,
            session_id: metadata.session_id.clone(),
            agent_key: metadata.agent_key.clone(),
            project_path: metadata.cwd.clone(),
            project_label: metadata.project_label.clone(),
            title: metadata.title.clone(),
            protocol_version: metadata.protocol_version.clone(),
            capabilities_json: metadata.capabilities_json.clone(),
            restorable: metadata.restorable,
            availability: Availability::Active,
            turn: 0,
            last_error: None,
        })
    }

    pub async fn update_title(&self, row: &SessionRow, title: Option<&str>) -> BotResult {
        let ctx = self.context()?;
        let name = post_title(&row.project_label, title, &row.session_id);
        let channel = row
            .thread_id
            .to_channel(ctx, Some(self.config.discord.guild_id))
            .await?;
        let Channel::GuildThread(thread) = channel else {
            return Err(BotError::Other("session thread no longer exists".into()));
        };
        if thread.base.name != name {
            ThreadId::new(row.thread_id.get())
                .edit(&ctx.http, EditThread::new().name(name))
                .await?;
        }
        self.db.set_title(row.thread_id, title)?;
        Ok(())
    }

    pub async fn update_availability(
        &self,
        row: &SessionRow,
        availability: Availability,
        error: Option<&str>,
    ) -> BotResult {
        let ctx = self.context()?;
        let agent = self.config.agents.get(&row.agent_key);
        let display_name = agent.map_or(row.agent_key.as_str(), |agent| &agent.display_name);
        let tags = self.tag_ids(ctx).await?;
        let applied: Vec<_> = tags.get(&row.agent_key).copied().into_iter().collect();
        ThreadId::new(row.thread_id.get())
            .edit(
                &ctx.http,
                EditThread::new()
                    .applied_tags(applied)
                    .archived(availability != Availability::Active),
            )
            .await?;
        self.db
            .set_availability(row.thread_id, availability, error)?;
        tracing::debug!(agent = %display_name, session = %row.session_id, ?availability, "updated session availability");
        Ok(())
    }

    pub async fn update_usage(&self, thread: GenericChannelId, usage: &UsageUpdate) -> BotResult {
        let ctx = self.context()?;
        let row = self
            .db
            .session(thread)?
            .ok_or_else(|| BotError::Other("session disappeared while updating usage".into()))?;
        let metadata = SessionMetadata {
            agent_key: row.agent_key,
            project_label: row.project_label,
            cwd: row.project_path,
            session_id: row.session_id,
            protocol_version: row.protocol_version,
            capabilities_json: row.capabilities_json,
            restorable: row.restorable,
            title: row.title,
        };
        let usage = usage_text(usage);
        let content = starter_message(&metadata, Some(&usage));
        row.thread_id
            .edit_message(
                &ctx.http,
                row.starter_message_id,
                EditMessage::new().content(content),
            )
            .await?;
        Ok(())
    }

    pub async fn delete_manual_post(&self, thread: &GuildThread) -> BotResult {
        let ctx = self.context()?;
        if thread.parent_id != self.config.discord.forum_channel_id {
            return Ok(());
        }
        if self.db.session(thread.id.widen())?.is_some() {
            return Ok(());
        }
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
        if starter.author.id != ctx.cache.current_user().id {
            thread.id.widen().delete(&ctx.http, None).await?;
        }
        Ok(())
    }

    async fn channel(&self, ctx: &Context) -> BotResult<GuildChannel> {
        let channel = self
            .config
            .discord
            .forum_channel_id
            .to_guild_channel(ctx, Some(self.config.discord.guild_id))
            .await?;
        if channel.base.kind == ChannelType::Forum {
            Ok(channel)
        } else {
            Err(BotError::Config(format!(
                "Discord channel {} is not a forum",
                self.config.discord.forum_channel_id
            )))
        }
    }

    async fn tag_ids(&self, ctx: &Context) -> BotResult<HashMap<String, ForumTagId>> {
        let mut channel = self.channel(ctx).await?;
        let configured_names = self
            .config
            .agents
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let mut desired = channel
            .available_tags
            .iter()
            .filter(|tag| !configured_names.contains(&tag.name.as_str()))
            .map(copy_tag)
            .collect::<Vec<_>>();
        desired.extend(
            self.config
                .agents
                .iter()
                .map(|(key, agent)| CreateForumTag::new(key).emoji(reaction(&agent.emoji))),
        );
        if desired.len() > 20 {
            return Err(BotError::Config(format!(
                "the forum's existing tags plus {} configured agent tags exceed Discord's 20-tag limit",
                self.config.agents.len()
            )));
        }

        let current = channel
            .available_tags
            .iter()
            .map(|tag| (tag.name.to_string(), emoji_key(tag.emoji.as_ref())))
            .collect::<Vec<_>>();
        let wanted = self
            .config
            .agents
            .iter()
            .map(|(key, agent)| (key.clone(), configured_emoji_key(&agent.emoji)))
            .collect::<Vec<_>>();
        let needs_update = wanted
            .iter()
            .any(|tag| !current.iter().any(|current| current == tag));
        if needs_update {
            channel
                .id
                .edit(&ctx.http, EditChannel::new().available_tags(desired))
                .await?;
            channel = self.channel(ctx).await?;
        }

        Ok(self
            .config
            .agents
            .keys()
            .filter_map(|key| {
                channel
                    .available_tags
                    .iter()
                    .find(|tag| &tag.name == key)
                    .map(|tag| (key.clone(), tag.id))
            })
            .collect())
    }
}

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

fn reaction(emoji: &TagEmoji) -> ReactionType {
    match emoji {
        TagEmoji::Unicode(value) => ReactionType::Unicode(value.clone().trunc_into()),
        TagEmoji::Custom { id, animated } => ReactionType::Custom {
            animated: *animated,
            id: EmojiId::new(*id),
            name: None,
        },
    }
}

fn emoji_key(emoji: Option<&ForumEmoji>) -> String {
    match emoji {
        Some(ForumEmoji::Id(id)) => format!("id:{}", id.get()),
        Some(ForumEmoji::Name(name)) => format!("unicode:{name}"),
        None => "none".into(),
        _ => "unknown".into(),
    }
}

fn configured_emoji_key(emoji: &TagEmoji) -> String {
    match emoji {
        TagEmoji::Unicode(value) => format!("unicode:{value}"),
        TagEmoji::Custom { id, .. } => format!("id:{id}"),
    }
}

#[must_use]
pub fn post_title(project: &str, title: Option<&str>, session_id: &str) -> String {
    let fallback = format!(
        "session {}",
        session_id.chars().take(12).collect::<String>()
    );
    let title = title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&fallback);
    let raw = format!("{project} · {title}")
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_end(raw.trim(), THREAD_TITLE_LIMIT)
}

fn starter_message(metadata: &SessionMetadata, usage: Option<&str>) -> String {
    let mut content = format!(
        "session `{}` · cwd `{}`",
        escape_inline(&metadata.session_id),
        escape_inline(&metadata.cwd),
    );
    if let Some(usage) = usage {
        content.push_str(" · usage `");
        content.push_str(&escape_inline(usage));
        content.push('`');
    }
    content
}

fn usage_text(usage: &UsageUpdate) -> String {
    let mut content = format!("{} / {} tokens", usage.used, usage.size);
    if let Some(cost) = &usage.cost {
        let _ = write!(content, " · {} {}", cost.amount, cost.currency);
    }
    content
}

fn escape_inline(value: &str) -> String {
    truncate_end(&value.replace('`', "ˋ").replace(['\n', '\r'], " "), 300)
}

pub fn truncate_end(value: &str, limit: usize) -> String {
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

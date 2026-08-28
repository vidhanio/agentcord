use std::{collections::HashMap, fmt::Write, path::Path};

use agent_client_protocol::schema::v1::{SessionConfigOptionCategory, UsageUpdate};
use serenity::all::{
    Channel, ChannelType, Context, CreateForumPost, CreateForumTag, CreateMessage, EditChannel,
    EditMessage, EditThread, EmojiId, ForumEmoji, ForumTag, ForumTagId, GenericChannelId,
    GetMessages, GuildChannel, GuildThread, MessageId, ReactionType, ThreadId,
    small_fixed_array::TruncatingInto,
};

use crate::{
    Bot, BotError, BotResult, acp::SessionUiState, config::TagEmoji, db::SessionRow, projects,
};

const THREAD_TITLE_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct SessionMetadata {
    pub agent_key: String,
    pub project_label: String,
    pub cwd: String,
    pub session_id: String,
    pub title: Option<String>,
}

impl Bot {
    pub async fn validate_and_reconcile_forum(&self) -> BotResult {
        let ctx = self.context()?;
        self.channel(ctx).await?;
        self.tag_ids(ctx).await?;
        Ok(())
    }

    /// Creates a session thread in the configured forum.
    pub async fn create_session_post(
        &self,
        metadata: &SessionMetadata,
        ui: Option<&SessionUiState>,
    ) -> BotResult<SessionRow> {
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
                    CreateMessage::new().content(starter_message(
                        &metadata.session_id,
                        &metadata.cwd,
                        ui,
                        None,
                    )),
                ),
            )
            .await?;
        ThreadId::new(created.id.get())
            .edit(&ctx.http, EditThread::new().applied_tags(vec![tag]))
            .await?;

        Ok(SessionRow {
            thread_id: created.id.widen(),
            session_id: metadata.session_id.clone(),
            agent_key: metadata.agent_key.clone(),
            project_path: metadata.cwd.clone(),
        })
    }

    pub async fn update_title(
        &self,
        thread: GenericChannelId,
        project_path: &str,
        session_id: &str,
        title: Option<&str>,
    ) -> BotResult {
        let ctx = self.context()?;
        let project = projects::adopt(&self.config.projects, Path::new(project_path));
        let name = post_title(&project.label, title, session_id);
        let channel = thread
            .to_channel(ctx, Some(self.config.discord.guild_id))
            .await?;
        let Channel::GuildThread(guild_thread) = channel else {
            return Err(BotError::Other("session thread no longer exists".into()));
        };
        if guild_thread.base.name != name {
            ThreadId::new(thread.get())
                .edit(&ctx.http, EditThread::new().name(name))
                .await?;
        }
        Ok(())
    }

    pub async fn set_thread_archived(
        &self,
        thread: GenericChannelId,
        agent_key: &str,
        archived: bool,
    ) -> BotResult {
        let ctx = self.context()?;
        let tags = self.tag_ids(ctx).await?;
        let applied: Vec<_> = tags.get(agent_key).copied().into_iter().collect();
        ThreadId::new(thread.get())
            .edit(
                &ctx.http,
                EditThread::new().applied_tags(applied).archived(archived),
            )
            .await?;
        tracing::debug!(%agent_key, ?thread, %archived, "updated session thread availability");
        Ok(())
    }

    pub async fn update_starter(
        &self,
        thread: GenericChannelId,
        ui: &SessionUiState,
        usage: Option<&UsageUpdate>,
    ) -> BotResult {
        let ctx = self.context()?;
        let row = self
            .db
            .session(thread)?
            .ok_or_else(|| BotError::Other("session disappeared while updating usage".into()))?;
        // Discord thread ids are the ids of their starter messages, including
        // posts created in forum channels.
        let starter = MessageId::new(thread.get());
        let usage = usage.map(usage_text);
        let content = starter_message(
            &row.session_id,
            &row.project_path,
            Some(ui),
            usage.as_deref(),
        );
        thread
            .edit_message(&ctx.http, starter, EditMessage::new().content(content))
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

fn starter_message(
    session_id: &str,
    cwd: &str,
    ui: Option<&SessionUiState>,
    usage: Option<&str>,
) -> String {
    let mut segments = vec![
        format!("session `{}`", escape_inline(session_id)),
        format!("cwd `{}`", escape_inline(cwd)),
    ];
    if let Some(ui) = ui {
        if let Some(mode) = ui.mode_label() {
            segments.push(format!("mode `{}`", escape_inline(&mode)));
        }
        if let Some(model) = ui.config_label(&SessionConfigOptionCategory::Model) {
            segments.push(format!("model `{}`", escape_inline(&model)));
        }
        if let Some(thought) = ui.config_label(&SessionConfigOptionCategory::ThoughtLevel) {
            segments.push(format!("thought `{}`", escape_inline(&thought)));
        }
    }
    if let Some(usage) = usage {
        segments.push(format!("usage `{}`", escape_inline(usage)));
    }
    segments.join(" · ")
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

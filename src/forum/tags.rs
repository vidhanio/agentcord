//! Forum tag management: the managed tag set (statuses + harnesses), its
//! reconciliation onto a forum channel, and the metadata writes that apply
//! tags (plus title and archive state) to a session post.

use std::collections::{HashMap, HashSet};

use serenity::all::{
    ChannelId, Context, CreateForumTag, EditChannel, EditThread, ForumEmoji, ForumTag, ForumTagId,
    GuildChannel, ReactionType, ThreadId, small_fixed_array::TruncatingInto,
};

use crate::{BotResult, forum::Forum, herdr::AgentStatus, session::Harness};

/// Emoji for harness tags.
const HARNESS_EMOJI: &str = "🤖";

/// Discord allows at most 20 tags per forum channel; the bot manages 5
/// status tags plus one tag per harness, well under the cap.
const TAG_STATUSES: [(AgentStatus, &str); 5] = [
    (AgentStatus::Idle, "⚪"),
    (AgentStatus::Working, "🟡"),
    (AgentStatus::Blocked, "🔴"),
    (AgentStatus::Done, "🟢"),
    (AgentStatus::Unknown, "⚫"),
];

impl Forum {
    /// Returns the id of every status and harness tag on `channel_id`, creating
    /// missing tags on demand: lifecycle-status tags get their canonical
    /// emoji, the harness tag gets the harness emoji. Stateless: the forum's
    /// tag list is fetched fresh on each call.
    /// Returns the id of every managed tag on `channel_id`: the lifecycle
    /// statuses and every harness, with their canonical emojis. The
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
                Harness::ALL
                    .iter()
                    .map(|harness| (harness.as_str(), HARNESS_EMOJI)),
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

    /// Applies `harness` + `status` tags and the post title to a session post
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
        harness: Option<Harness>,
        status: AgentStatus,
        title: Option<&str>,
    ) -> BotResult<()> {
        let ids = self.tag_ids(ctx, forum).await?;

        let mut applied = Vec::new();
        if let Some(harness) = harness
            && let Some(id) = ids.get(harness.as_str())
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

    /// Applies only the harness tag to a dead session's post: the status
    /// tag is dropped; the thread itself is closed (archived — a message
    /// still unarchives it and resumes the session).
    pub(crate) async fn dead_post_tags(
        &self,
        ctx: &Context,
        forum: ChannelId,
        post: ChannelId,
    ) -> BotResult<()> {
        let ids = self.tag_ids(ctx, forum).await?;
        let harness_id = self
            .applied_harness(ctx, post)
            .await?
            .and_then(|harness| ids.get(harness.as_str()).copied());
        let applied = harness_id.into_iter().collect::<Vec<_>>();
        ThreadId::new(post.get())
            .edit(&ctx.http, EditThread::new().applied_tags(applied))
            .await?;
        Ok(())
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
pub fn tag_names(forum: &GuildChannel) -> HashMap<ForumTagId, &str> {
    forum
        .available_tags
        .iter()
        .map(|tag| (tag.id, tag.name.as_str()))
        .collect()
}

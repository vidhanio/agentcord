//! Resolving Discord channels for the bot's bindings: a forum channel from
//! its id, a forum post (thread) from its id, the forum containing a post,
//! and existence checks.

use serenity::all::{Channel, ChannelId, Context, GuildChannel, GuildThread};

use crate::{BotResult, error::BotError, forum::Forum};

impl Forum {
    /// Whether `channel_id` still exists on Discord; `false` when it was
    /// deleted. Other failures propagate.
    pub async fn channel_exists(&self, ctx: &Context, channel_id: ChannelId) -> BotResult<bool> {
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

    /// The forum channel containing `post` (its parent channel).
    pub async fn forum_for_post(&self, ctx: &Context, post: ChannelId) -> BotResult<ChannelId> {
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

    pub async fn forum_channel(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
    ) -> BotResult<GuildChannel> {
        match ctx.http.get_channel(channel_id.widen()).await? {
            Channel::Guild(channel) => Ok(channel),
            _ => Err(BotError::ForumChannelNotFound),
        }
    }

    /// The thread channel `thread_id` (a forum post) as a
    /// [`GuildThread`], whose parent is the forum channel.
    pub async fn forum_thread(
        &self,
        ctx: &Context,
        thread_id: ChannelId,
    ) -> BotResult<GuildThread> {
        match ctx.http.get_channel(thread_id.widen()).await? {
            Channel::GuildThread(thread) => Ok(thread),
            _ => Err(BotError::ForumChannelNotFound),
        }
    }
}

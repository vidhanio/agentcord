//! Webhook echoes: mirroring user turns that were typed in herdr (and never
//! appeared in Discord) into session posts through a per-forum webhook named
//! and avatared after the allowed user.

use serenity::all::{
    ChannelId, Context, CreateMessage, CreateWebhook, ExecuteWebhook, GetMessages, MessageId,
    ThreadId, Webhook,
};
use tracing::warn;

use crate::{
    BotError, BotResult,
    forum::{Forum, render::split_lines},
};

/// The allowed user's Discord profile, used to name and avatar the webhook
/// that mirrors their turns into session posts.
#[derive(Debug, Clone)]
struct UserProfile {
    username: String,
    avatar_url: Option<String>,
}

impl Forum {
    /// Posts a user turn as an echo unless the user already typed it into
    /// the thread. Echoes go through the forum's user webhook (named and
    /// avatared after the allowed user) so transcript turns look like the
    /// user's own messages; falls back to plain bot messages when no
    /// webhook is available. Text longer than Discord's limit is split at
    /// line boundaries like agent messages — an overlong echo must never
    /// wedge the sync cursor on a `TooLarge` error. Returns the last
    /// posted message id, or `None` when the echo was skipped.
    pub async fn post_user_echo(
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
        let recent = post.widen().messages(&ctx.http, builder).await?;
        if recent.iter().any(|message| message.content == text) {
            return Ok(None);
        }

        let chunks = split_lines(text, serenity::constants::MESSAGE_CODE_LIMIT);

        // Echo the chunks through the user webhook; whatever the webhook
        // did not post (unavailable, or a failure part-way) falls back to
        // plain bot messages, so a long turn can never wedge the cursor.
        let mut posted: Vec<MessageId> = Vec::new();
        if let Some(profile) = self.user_profile(ctx).await
            && let Some(webhook) = self.user_webhook(ctx, forum).await
        {
            for chunk in &chunks {
                let mut builder = ExecuteWebhook::new()
                    .content(chunk)
                    .in_thread(ThreadId::new(post.get()))
                    .username(&profile.username);
                if let Some(avatar_url) = &profile.avatar_url {
                    builder = builder.avatar_url(avatar_url.clone());
                }
                match webhook.execute(&ctx.http, true, builder).await {
                    Ok(Some(message)) => posted.push(message.id),
                    Ok(None) => {
                        warn!("user webhook returned no message, falling back to bot echo");
                        break;
                    }
                    Err(error) => {
                        warn!(?error, "user webhook echo failed, falling back to bot echo");
                        break;
                    }
                }
            }
        }
        for chunk in chunks.iter().skip(posted.len()) {
            posted.push(
                post.widen()
                    .send_message(&ctx.http, CreateMessage::new().content(chunk))
                    .await?
                    .id,
            );
        }
        Ok(posted.last().copied())
    }

    /// The allowed user's webhook persona (guild nickname or display name,
    /// plus avatar URL). Fetched fresh on each echo. `None` when the
    /// fetch fails.
    async fn user_profile(&self, ctx: &Context) -> Option<UserProfile> {
        let user_id = self.config.discord.allowed_user_id;
        let user = ctx.http.get_user(user_id).await.ok()?;
        // The guild nickname takes priority; fall back to the global
        // display name. The "(via herdr)" suffix marks webhook echoes.
        let name = match ctx
            .http
            .get_member(self.config.discord.guild_id, user_id)
            .await
        {
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
            .webhooks(&ctx.http)
            .await
            .ok()?
            .into_iter()
            .find(|webhook| webhook.name.as_deref() == Some(profile.username.as_str()));
        match existing {
            Some(webhook) => Some(webhook),
            None => match forum
                .create_webhook(&ctx.http, CreateWebhook::new(&profile.username))
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
}

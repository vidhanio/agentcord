use serenity::all::{
    CreateMessage, CreateWebhook, ExecuteWebhook, GenericChannelId, ThreadId, Webhook,
};
use tracing::warn;

use crate::{Bot, BotResult, render::split_message};

#[derive(Debug, Clone)]
struct UserProfile {
    username: String,
    avatar_url: Option<String>,
}

impl Bot {
    pub async fn post_user_message(&self, thread: GenericChannelId, text: &str) -> BotResult {
        let ctx = self.context()?.clone();
        let chunks = split_message(text, serenity::constants::MESSAGE_CODE_LIMIT);
        let profile = self.user_profile(&ctx).await;
        let webhook = match profile.as_ref() {
            Some(profile) => {
                self.user_webhook(&ctx, self.config.discord.forum_channel_id, profile)
                    .await
            }
            None => None,
        };

        let mut posted = 0;
        if let (Some(profile), Some(webhook)) = (profile, webhook) {
            for chunk in &chunks {
                let mut builder = ExecuteWebhook::new()
                    .content(chunk)
                    .in_thread(ThreadId::new(thread.get()))
                    .username(profile.username.clone());
                if let Some(avatar_url) = &profile.avatar_url {
                    builder = builder.avatar_url(avatar_url.clone());
                }
                match webhook.execute(&ctx.http, true, builder).await {
                    Ok(Some(_)) => posted += 1,
                    Ok(None) => {
                        warn!("user webhook returned no message, falling back to a bot message");
                        break;
                    }
                    Err(error) => {
                        warn!(?error, "user webhook failed, falling back to a bot message");
                        break;
                    }
                }
            }
        }
        for chunk in chunks.iter().skip(posted) {
            thread
                .send_message(&ctx.http, CreateMessage::new().content(chunk))
                .await?;
        }
        Ok(())
    }

    async fn user_profile(&self, ctx: &serenity::all::Context) -> Option<UserProfile> {
        let user_id = self.config.discord.allowed_user_id;
        let user = ctx.http.get_user(user_id).await.ok()?;
        let name = match ctx
            .http
            .get_member(self.config.discord.guild_id, user_id)
            .await
        {
            Ok(member) => member.display_name().to_owned(),
            Err(_) => user.global_name.as_deref().unwrap_or(&user.name).to_owned(),
        };
        Some(UserProfile {
            username: format!("{name} (via agentcord)"),
            avatar_url: user.avatar_url(),
        })
    }

    async fn user_webhook(
        &self,
        ctx: &serenity::all::Context,
        forum: serenity::all::ChannelId,
        profile: &UserProfile,
    ) -> Option<Webhook> {
        let _guard = self.webhook_lock.lock().await;
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

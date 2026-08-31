//! Discord webhook mirroring for user-authored prompts.

use serenity::all::{
    CreateMessage, CreateWebhook, ExecuteWebhook, GenericChannelId, ThreadId, Webhook,
};
use tracing::warn;

use crate::{Bot, BotResult, discord::render::split_message};

/// Name used to identify Agentcord's forum webhook.
const WEBHOOK_NAME: &str = "agentcord";

/// Display identity used for webhook-authored prompt messages.
#[derive(Clone, Debug)]
struct UserProfile {
    /// Name shown on webhook-authored messages.
    username: String,
    /// Optional avatar URL copied from the Discord user.
    avatar_url: Option<String>,
}

impl UserProfile {
    /// Fetches the configured user's guild display name and avatar.
    async fn fetch(
        context: &serenity::all::Context,
        guild: serenity::all::GuildId,
        user_id: serenity::all::UserId,
    ) -> Option<Self> {
        let user = match context.http.get_user(user_id).await {
            Ok(user) => user,
            Err(error) => {
                warn!(?error, %user_id, "failed to fetch user profile");
                return None;
            }
        };
        let name = match context.http.get_member(guild, user_id).await {
            Ok(member) => member.display_name().to_owned(),
            Err(error) => {
                warn!(?error, %guild, %user_id, "failed to fetch guild member; using global profile");
                user.global_name.as_deref().unwrap_or(&user.name).to_owned()
            }
        };
        Some(Self {
            username: format!("{name} (via agentcord)"),
            avatar_url: user.avatar_url(),
        })
    }
}

impl Bot {
    /// Checks that the prompt webhook exists and caches its handle.
    pub(crate) async fn validate_and_reconcile_webhook(&self) -> BotResult {
        let context = self.context()?.clone();
        if self.user_webhook(&context).await.is_none() {
            warn!("prompt webhook is unavailable");
        }
        Ok(())
    }

    /// Mirrors a prompt as the configured Discord user.
    ///
    /// Webhook failures are non-fatal: messages not sent by the webhook are
    /// sent with the bot identity so ACP receives only after the prompt is
    /// visible in the thread.
    pub async fn mirror_user_message(&self, thread: GenericChannelId, text: &str) -> BotResult {
        let context = self.context()?.clone();
        let chunks = split_message(text, serenity::constants::MESSAGE_CODE_LIMIT);
        let profile = UserProfile::fetch(
            &context,
            self.config().discord.guild_id,
            self.config().discord.allowed_user_id,
        )
        .await;
        let webhook = match profile.as_ref() {
            Some(_) => self.user_webhook(&context).await,
            None => None,
        };

        let mut posted = 0;
        if let (Some(profile), Some(webhook)) = (profile, webhook) {
            for chunk in &chunks {
                let mut request = ExecuteWebhook::new()
                    .content(chunk)
                    .in_thread(ThreadId::new(thread.get()))
                    .username(profile.username.clone());
                if let Some(avatar_url) = &profile.avatar_url {
                    request = request.avatar_url(avatar_url.clone());
                }
                match webhook.execute(&context.http, true, request).await {
                    Ok(Some(_)) => posted += 1,
                    Ok(None) => {
                        warn!("user webhook returned no message; using bot fallback");
                        break;
                    }
                    Err(error) => {
                        warn!(?error, "user webhook failed; using bot fallback");
                        *self.webhook().lock().await = None;
                        break;
                    }
                }
            }
        }

        for chunk in chunks.iter().skip(posted) {
            thread
                .send_message(&context.http, CreateMessage::new().content(chunk))
                .await?;
        }
        Ok(())
    }

    /// Finds or creates the forum webhook used for mirrored prompts.
    async fn user_webhook(&self, context: &serenity::all::Context) -> Option<Webhook> {
        let mut cached = self.webhook().lock().await;
        if let Some(webhook) = cached.as_ref() {
            let webhook = webhook.clone();
            drop(cached);
            return Some(webhook);
        }

        let forum = self.config().discord.forum_channel_id;
        let existing = match forum.webhooks(&context.http).await {
            Ok(webhooks) => webhooks
                .into_iter()
                .find(|webhook| webhook.name.as_deref() == Some(WEBHOOK_NAME)),
            Err(error) => {
                warn!(?error, %forum, "failed to list prompt webhooks");
                return None;
            }
        };
        let webhook = match existing {
            Some(webhook) => Ok(webhook),
            None => {
                forum
                    .create_webhook(&context.http, CreateWebhook::new(WEBHOOK_NAME))
                    .await
            }
        };
        let result = match webhook {
            Ok(webhook) => {
                *cached = Some(webhook.clone());
                Some(webhook)
            }
            Err(error) => {
                warn!(?error, %forum, "failed to find or create prompt webhook");
                None
            }
        };
        drop(cached);
        result
    }

    /// Returns the mutex protecting the process-wide webhook cache.
    pub(crate) fn webhook(&self) -> &tokio::sync::Mutex<Option<Webhook>> {
        self.state().discord.webhook()
    }
}

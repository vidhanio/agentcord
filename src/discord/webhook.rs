//! Discord webhook mirroring for user-authored prompts.

use serenity::all::{
    Context, CreateMessage, CreateWebhook, EditWebhookMessage, ExecuteWebhook, GenericChannelId,
    MessageId, ThreadId, Webhook,
};
use tracing::{debug, info, warn};

use crate::{
    Bot, BotResult,
    discord::render::{SyncFailure, plan_messages, send_messages, split_message, sync_messages},
};

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
        debug!(%user_id, "fetching discord user profile...");
        let user = match context.http.get_user(user_id).await {
            Ok(user) => {
                debug!(%user_id, "fetched discord user profile");
                user
            }
            Err(error) => {
                warn!(?error, %user_id, "failed to fetch user profile");
                return None;
            }
        };
        debug!(%guild, %user_id, "fetching discord guild member...");
        let name = match context.http.get_member(guild, user_id).await {
            Ok(member) => {
                debug!(%guild, %user_id, "fetched discord guild member");
                member.display_name().to_owned()
            }
            Err(error) => {
                warn!(?error, %guild, %user_id, "failed to fetch guild member; using global profile...");
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
        info!("validating prompt webhook...");
        let context = self.context()?.clone();
        if let Some(webhook) = self.user_webhook(&context).await {
            info!(webhook = ?webhook.id, "prompt webhook is ready");
        } else {
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
        info!(
            thread = ?thread,
            chunks = chunks.len(),
            "mirroring user message..."
        );
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

        let posted = match (profile, webhook) {
            (Some(profile), Some(webhook)) => {
                self.send_webhook_chunks(&context, thread, &chunks, profile, &webhook)
                    .await
            }
            _ => Vec::new(),
        };

        let fallback = chunks.len().saturating_sub(posted.len());
        if fallback > 0 {
            info!(
                thread = ?thread,
                webhook_chunks = posted.len(),
                fallback_chunks = fallback,
                "sending user message with bot identity..."
            );
        }
        self.send_bot_chunks(&context, thread, &chunks, posted.len())
            .await?;
        info!(
            thread = ?thread,
            webhook_chunks = posted.len(),
            fallback_chunks = fallback,
            "mirrored user message"
        );
        Ok(())
    }

    /// Synchronizes one replayed ACP user message through the prompt webhook.
    pub(crate) async fn sync_user_message_projection(
        &self,
        context: &Context,
        thread: GenericChannelId,
        previous: &[MessageId],
        chunks: Vec<String>,
    ) -> Result<Vec<MessageId>, SyncFailure> {
        let plan = plan_messages(previous, &chunks);
        let profile = UserProfile::fetch(
            context,
            self.config().discord.guild_id,
            self.config().discord.allowed_user_id,
        )
        .await;
        let webhook = match profile.as_ref() {
            Some(_) => self.user_webhook(context).await,
            None => None,
        };
        let Some((profile, webhook)) = profile.zip(webhook) else {
            debug!(
                thread = ?thread,
                "prompt webhook unavailable; synchronizing replayed user message with bot identity"
            );
            return sync_messages(context, thread, previous, chunks).await;
        };

        debug!(
            thread = ?thread,
            webhook = ?webhook.id,
            edits = plan.edits.len(),
            sends = plan.sends.len(),
            deletes = plan.deletes.len(),
            "synchronizing replayed user message through webhook..."
        );
        let current = match Self::edit_webhook_messages(
            context,
            thread,
            previous,
            &plan.edits,
            &webhook,
        )
        .await
        {
            Ok(current) => current,
            Err(error) => {
                warn!(
                    error = ?error.error,
                    thread = ?thread,
                    webhook = ?webhook.id,
                    "failed to edit replayed user message through webhook; trying bot identity..."
                );
                return sync_messages(context, thread, previous, chunks).await;
            }
        };
        let posted = self
            .send_webhook_chunks(context, thread, &plan.sends, profile, &webhook)
            .await;
        let mut current = current;
        current.extend(posted.iter().copied());
        if posted.len() < plan.sends.len() {
            current = send_messages(context, thread, current, &plan.sends[posted.len()..]).await?;
        }
        Self::delete_webhook_messages(context, thread, current, &plan.deletes, &webhook).await
    }

    /// Sends as many chunks as possible through the user's webhook identity.
    async fn send_webhook_chunks(
        &self,
        context: &serenity::all::Context,
        thread: GenericChannelId,
        chunks: &[String],
        profile: UserProfile,
        webhook: &Webhook,
    ) -> Vec<MessageId> {
        let mut posted = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            debug!(
                thread = ?thread,
                webhook = ?webhook.id,
                chunk = index + 1,
                chunks = chunks.len(),
                characters = chunk.chars().count(),
                "sending user message through webhook..."
            );
            let mut request = ExecuteWebhook::new()
                .content(chunk)
                .in_thread(ThreadId::new(thread.get()))
                .username(profile.username.clone());
            if let Some(avatar_url) = &profile.avatar_url {
                request = request.avatar_url(avatar_url.clone());
            }
            match webhook.execute(&context.http, true, request).await {
                Ok(Some(message)) => {
                    posted.push(message.id);
                    debug!(
                        thread = ?thread,
                        webhook = ?webhook.id,
                        message = ?message.id,
                        chunk = index + 1,
                        "sent user message through webhook"
                    );
                }
                Ok(None) => {
                    warn!(
                        thread = ?thread,
                        webhook = ?webhook.id,
                        chunk = index + 1,
                        "user webhook returned no message; using bot fallback..."
                    );
                    break;
                }
                Err(error) => {
                    warn!(
                        ?error,
                        thread = ?thread,
                        webhook = ?webhook.id,
                        chunk = index + 1,
                        "user webhook failed; using bot fallback..."
                    );
                    debug!("clearing cached prompt webhook...");
                    *self.webhook().lock().await = None;
                    debug!("cleared cached prompt webhook");
                    break;
                }
            }
        }
        posted
    }

    /// Edits existing replayed user messages through the prompt webhook.
    async fn edit_webhook_messages(
        context: &Context,
        thread: GenericChannelId,
        previous: &[MessageId],
        edits: &[(MessageId, String)],
        webhook: &Webhook,
    ) -> Result<Vec<MessageId>, SyncFailure> {
        let current = previous[..edits.len()].to_vec();
        for (id, chunk) in edits {
            debug!(
                thread = ?thread,
                webhook = ?webhook.id,
                message = ?id,
                characters = chunk.chars().count(),
                "editing replayed user message through webhook..."
            );
            if let Err(error) = webhook
                .edit_message(
                    &context.http,
                    *id,
                    EditWebhookMessage::new()
                        .content(chunk)
                        .in_thread(ThreadId::new(thread.get())),
                )
                .await
            {
                warn!(
                    ?error,
                    thread = ?thread,
                    webhook = ?webhook.id,
                    message = ?id,
                    "failed to edit replayed user message through webhook"
                );
                return Err(SyncFailure {
                    message_ids: previous.to_vec(),
                    error: Box::new(error.into()),
                });
            }
            debug!(
                thread = ?thread,
                webhook = ?webhook.id,
                message = ?id,
                "edited replayed user message through webhook"
            );
        }
        Ok(current)
    }

    /// Deletes stale replayed user messages through the prompt webhook.
    async fn delete_webhook_messages(
        context: &Context,
        thread: GenericChannelId,
        current: Vec<MessageId>,
        deletes: &[MessageId],
        webhook: &Webhook,
    ) -> Result<Vec<MessageId>, SyncFailure> {
        for (index, id) in deletes.iter().enumerate() {
            debug!(
                thread = ?thread,
                webhook = ?webhook.id,
                message = ?id,
                "deleting stale replayed user message through webhook..."
            );
            if let Err(error) = webhook
                .delete_message(&context.http, Some(ThreadId::new(thread.get())), *id)
                .await
            {
                warn!(
                    ?error,
                    thread = ?thread,
                    webhook = ?webhook.id,
                    message = ?id,
                    "failed to delete stale replayed user message through webhook"
                );
                let mut message_ids = current.clone();
                message_ids.extend_from_slice(&deletes[index..]);
                return Err(SyncFailure {
                    message_ids,
                    error: Box::new(error.into()),
                });
            }
            debug!(
                thread = ?thread,
                webhook = ?webhook.id,
                message = ?id,
                "deleted stale replayed user message through webhook"
            );
        }
        Ok(current)
    }

    /// Sends chunks not handled by the user's webhook with the bot identity.
    async fn send_bot_chunks(
        &self,
        context: &serenity::all::Context,
        thread: GenericChannelId,
        chunks: &[String],
        posted: usize,
    ) -> BotResult {
        for (index, chunk) in chunks.iter().skip(posted).enumerate() {
            let chunk_number = posted + index + 1;
            debug!(
                thread = ?thread,
                chunk = chunk_number,
                chunks = chunks.len(),
                characters = chunk.chars().count(),
                "sending user message with bot identity..."
            );
            let message = match thread
                .send_message(&context.http, CreateMessage::new().content(chunk))
                .await
            {
                Ok(message) => message,
                Err(error) => {
                    warn!(
                        ?error,
                        thread = ?thread,
                        chunk = chunk_number,
                        "failed to send user message with bot identity"
                    );
                    return Err(error.into());
                }
            };
            debug!(
                thread = ?thread,
                message = ?message.id,
                chunk = chunk_number,
                "sent user message with bot identity"
            );
        }
        Ok(())
    }

    /// Finds or creates the forum webhook used for mirrored prompts.
    async fn user_webhook(&self, context: &serenity::all::Context) -> Option<Webhook> {
        let mut cached = self.webhook().lock().await;
        if let Some(webhook) = cached.as_ref() {
            let webhook = webhook.clone();
            drop(cached);
            debug!(webhook = ?webhook.id, "using cached prompt webhook");
            return Some(webhook);
        }

        let forum = self.config().discord.forum_channel_id;
        debug!(forum = %forum, "listing prompt webhooks...");
        let existing = match forum.webhooks(&context.http).await {
            Ok(webhooks) => {
                let available = webhooks.len();
                let existing = webhooks
                    .into_iter()
                    .find(|webhook| webhook.name.as_deref() == Some(WEBHOOK_NAME));
                debug!(
                    %forum,
                    available,
                    found = existing.is_some(),
                    "listed prompt webhooks"
                );
                existing
            }
            Err(error) => {
                warn!(?error, %forum, "failed to list prompt webhooks");
                return None;
            }
        };
        let webhook = if let Some(webhook) = existing {
            debug!(webhook = ?webhook.id, "using existing prompt webhook");
            Ok(webhook)
        } else {
            info!(%forum, "creating prompt webhook...");
            match forum
                .create_webhook(&context.http, CreateWebhook::new(WEBHOOK_NAME))
                .await
            {
                Ok(webhook) => {
                    info!(%forum, webhook = ?webhook.id, "created prompt webhook");
                    Ok(webhook)
                }
                Err(error) => Err(error),
            }
        };
        let result = match webhook {
            Ok(webhook) => {
                *cached = Some(webhook.clone());
                debug!(webhook = ?webhook.id, "cached prompt webhook");
                Some(webhook)
            }
            Err(error) => {
                warn!(?error, %forum, "failed to create prompt webhook");
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

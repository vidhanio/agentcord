//! Minimal slash-command surface for creating and importing sessions.

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use tracing::warn;

use crate::{Bot, BotError};

const CHOICE_LIMIT: usize = 25;

/// Serenity framework adapter around the Poise command set.
pub struct BotFramework {
    poise: poise::Framework<Bot, BotError>,
    guild_id: serenity::GuildId,
}

/// Builds the guild-scoped command framework.
pub fn framework(bot: &Bot) -> BotFramework {
    BotFramework {
        poise: poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![agent::agent(), import::import()],
                on_error,
                ..Default::default()
            })
            .build(),
        guild_id: bot.config().discord.guild_id,
    }
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
    }

    async fn dispatch(&self, context: &serenity::Context, event: &serenity::FullEvent) {
        if let serenity::FullEvent::Ready { data_about_bot, .. } = event {
            context
                .http
                .set_application_id(data_about_bot.application.id);
            if let Err(error) = poise::builtins::register_in_guild(
                &context.http,
                &self.poise.options().commands,
                self.guild_id,
            )
            .await
            {
                warn!(?error, "failed to register slash commands");
            }
        }
        self.poise.dispatch(context, event).await;
    }
}

#[expect(clippy::unused_async, reason = "Poise checks must be async functions")]
/// Restricts commands to the configured Discord user.
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.author().id == ctx.data().config().discord.allowed_user_id)
}

/// Converts framework errors into concise ephemeral command replies.
fn on_error(error: FrameworkError<'_, Bot, BotError>) -> poise::BoxFuture<'_, ()> {
    Box::pin(async move {
        match error {
            FrameworkError::Command { error, ctx, .. } => {
                let _ = ctx
                    .send(
                        CreateReply::new()
                            .content(format!("command failed: {error}"))
                            .ephemeral(true),
                    )
                    .await;
            }
            FrameworkError::CommandCheckFailed { ctx, .. } => {
                let _ = ctx
                    .send(
                        CreateReply::new()
                            .content("you're not allowed to use this bot")
                            .ephemeral(true),
                    )
                    .await;
            }
            other => {
                if let Err(error) = poise::builtins::on_error(other).await {
                    warn!(?error, "error while handling command framework event");
                }
            }
        }
    })
}

/// Truncates a Discord choice label without splitting Unicode characters.
fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut value = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    value.push('…');
    value
}

mod agent;
mod import;

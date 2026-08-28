use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use tracing::warn;

use crate::{Bot, BotError};

/// Maximum autocomplete choices Discord accepts.
const CHOICE_LIMIT: usize = 25;

#[must_use]
/// Truncates Discord choice labels without splitting Unicode characters.
fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(limit - 1).collect::<String>();
    truncated.push('…');
    truncated
}

/// Serenity framework adapter around Poise and guild registration state.
pub struct BotFramework {
    /// Wrapped command framework.
    poise: poise::Framework<Bot, BotError>,
    /// Guild where slash commands are registered.
    guild_id: serenity::GuildId,
}

/// Builds the Poise framework and registers Agentcord's command set.
pub fn framework(bot: &Bot) -> BotFramework {
    BotFramework {
        poise: poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![
                    agent::agent(),
                    import::import(),
                    command::command(),
                    mode::mode(),
                    model::model(),
                ],
                on_error,
                ..Default::default()
            })
            .build(),
        guild_id: bot.config.discord.guild_id,
    }
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    /// Initializes the wrapped Poise framework.
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
    }

    /// Registers commands on ready and forwards all events to Poise.
    async fn dispatch(&self, ctx: &serenity::Context, event: &serenity::FullEvent) {
        if let serenity::FullEvent::Ready { data_about_bot, .. } = event {
            ctx.http.set_application_id(data_about_bot.application.id);
            if let Err(error) = poise::builtins::register_in_guild(
                &ctx.http,
                &self.poise.options().commands,
                self.guild_id,
            )
            .await
            {
                warn!(?error, "failed to register slash commands");
            }
        }
        self.poise.dispatch(ctx, event).await;
    }
}

#[expect(
    clippy::unused_async,
    reason = "poise checks require an async function"
)]
/// Restricts every command to the configured Discord user.
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.data().is_allowed(ctx.author().id))
}

/// Converts framework failures into concise Discord responses or logs.
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
                    warn!(?error, "error while handling framework error");
                }
            }
        }
    })
}

mod agent;
mod command;
mod import;
mod mode;
mod model;

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use tracing::warn;

use crate::{Bot, BotError};

pub struct BotFramework {
    poise: poise::Framework<Bot, BotError>,
    guild_id: serenity::GuildId,
}

pub fn framework(bot: &Bot) -> BotFramework {
    BotFramework {
        poise: poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![agent::agent()],
                on_error,
                ..Default::default()
            })
            .build(),
        guild_id: bot.config.discord.guild_id,
    }
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
    }

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
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.data().is_allowed(ctx.author().id))
}

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

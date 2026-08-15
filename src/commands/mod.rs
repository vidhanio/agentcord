//! The poise framework wiring: the guild-command registry, the
//! allowed-user gate, and the error reporter. The commands themselves live
//! in `agent` (`/agent`, the modal) and `herdr` (`/herdr`, the control
//! command).

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use tracing::warn;

use crate::{Bot, BotError};

/// The poise framework for the bot: registers the guild commands and
/// dispatches interactions to them, while the bot's own event handler
/// keeps handling messages, threads, and the lifecycle.
pub struct BotFramework {
    poise: poise::Framework<Bot, BotError>,
    guild_id: serenity::GuildId,
}

/// Builds the framework over `bot`. The guild id is captured for
/// guild-only command registration (no user install needed).
pub fn framework(bot: &Bot) -> BotFramework {
    let poise_framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: build_commands(&bot.config),
            on_error,
            ..Default::default()
        })
        .build();
    BotFramework {
        poise: poise_framework,
        guild_id: bot.config.guild_id,
    }
}

/// The guild commands to register, in order. `/herdr` is only registered
/// when a control command is configured (`HERDR_CONTROL_COMMAND`) — the
/// bot stays inert without one.
fn build_commands(config: &crate::config::Config) -> Vec<poise::Command<Bot, BotError>> {
    let mut commands = vec![agent::agent()];
    if config.herdr_control_command.is_some() {
        commands.push(herdr::herdr());
    }
    commands
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
    }

    async fn dispatch(&self, ctx: &serenity::Context, event: &serenity::FullEvent) {
        // Command registration needs the application id on the HTTP
        // client; the Ready payload is the first reliable source for it,
        // so the (idempotent) guild registration happens here instead of
        // `init`.
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

/// The allowed-user gate for every command: everyone when no allowed user
/// is configured, otherwise only that user.
#[expect(
    clippy::unused_async,
    reason = "poise wraps check fns into a BoxFuture, so the async is required"
)]
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.data().is_allowed(ctx.author().id))
}

/// Reports command failures and the allowed-user gate to the user.
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
                            .content("you're not allowed to use this bot.")
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
mod herdr;

#[cfg(test)]
mod tests {
    use super::build_commands;
    use crate::test_util::control_config;

    #[test]
    fn build_commands_omits_herdr_without_a_control_command() {
        let config = control_config(None, None, None);
        let commands = build_commands(&config);
        let names = commands
            .into_iter()
            .map(|command| command.name.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["agent"]);
    }

    #[test]
    fn build_commands_registers_herdr_with_a_control_command() {
        let config = control_config(Some("cat"), None, None);
        let commands = build_commands(&config);
        let names = commands
            .into_iter()
            .map(|command| command.name.into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["agent", "herdr"]);
    }
}

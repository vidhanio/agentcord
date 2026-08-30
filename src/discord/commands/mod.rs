//! Minimal slash-command surface for creating and importing sessions.

use std::borrow::Cow;

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use tracing::warn;

use crate::{Bot, BotError, config::AgentKey};

/// Maximum number of choices Discord accepts in one autocomplete response.
const CHOICE_LIMIT: usize = 25;

/// Serenity framework adapter around the Poise command set.
pub struct BotFramework {
    /// Poise framework that parses and dispatches slash commands.
    poise: poise::Framework<Bot, BotError>,
    /// Guild where the commands are registered.
    guild_id: serenity::GuildId,
}

/// Builds the guild-scoped command framework.
pub fn framework(bot: &Bot) -> BotFramework {
    let mut agent = agent::agent();
    configure_agent_choices(&mut agent, bot);
    let mut import = import::import();
    configure_agent_choices(&mut import, bot);
    BotFramework {
        poise: poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![agent, import, model::model()],
                on_error,
                ..Default::default()
            })
            .build(),
        guild_id: bot.config().discord.guild_id,
    }
}

/// Adds the configured agents as fixed Discord choices to a command's agent
/// parameter.
fn configure_agent_choices(command: &mut poise::Command<Bot, BotError>, bot: &Bot) {
    let choices = bot
        .config()
        .agents
        .values()
        .map(|agent| poise::CommandParameterChoice {
            name: Cow::Owned(agent.display_name.clone()),
            localizations: Cow::Borrowed(&[]),
            __non_exhaustive: (),
        })
        .collect::<Vec<_>>();
    if let Some(parameter) = command
        .parameters
        .iter_mut()
        .find(|parameter| parameter.name == "agent")
    {
        parameter.autocomplete_callback = None;
        parameter.choices = choices.into();
    }
}

/// Resolves a fixed agent-choice index against the immutable configuration.
fn agent_key_at(bot: &Bot, index: usize) -> Result<AgentKey, BotError> {
    bot.config()
        .agents
        .keys()
        .nth(index)
        .cloned()
        .ok_or_else(|| BotError::UnknownAgent {
            key: format!("choice {index}"),
        })
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    /// Initializes Poise with the Discord client.
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
    }

    /// Registers commands on ready and forwards all gateway events to Poise.
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

#[expect(clippy::unused_async, reason = "poise checks must be async functions")]
/// Restricts commands to the configured Discord user.
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.author().id == ctx.data().config().discord.allowed_user_id)
}

/// Converts framework errors into concise ephemeral command replies.
fn on_error(error: FrameworkError<'_, Bot, BotError>) -> poise::BoxFuture<'_, ()> {
    Box::pin(async move {
        match error {
            FrameworkError::Command { error, ctx, .. } => {
                warn!(?error, "command failed");
                if let Err(send_error) = ctx
                    .send(
                        CreateReply::new()
                            .content(format!("command failed: {error}"))
                            .ephemeral(true),
                    )
                    .await
                {
                    warn!(?send_error, "failed to send command error response");
                }
            }
            FrameworkError::CommandCheckFailed { ctx, .. } => {
                if let Err(send_error) = ctx
                    .send(
                        CreateReply::new()
                            .content("you're not allowed to use this bot")
                            .ephemeral(true),
                    )
                    .await
                {
                    warn!(?send_error, "failed to send command permission response");
                }
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
mod model;

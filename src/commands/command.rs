use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use crate::{Bot, BotError};

/// Run one of the agent's advertised slash commands in this thread
#[poise::command(slash_command, check = "super::allowed")]
pub async fn command(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "agent command to run"]
    #[autocomplete = "command_choices"]
    cmd: String,
    #[description = "input for the command"] input: Option<String>,
) -> Result<(), BotError> {
    let bot = (*ctx.data()).clone();
    let thread = ctx.channel_id();
    let cmd = cmd.trim().trim_start_matches('/').to_owned();
    if cmd.is_empty() {
        return Err(BotError::Other("the command name is empty".into()));
    }
    let input = input
        .as_deref()
        .map(str::trim)
        .filter(|input| !input.is_empty());
    let prompt = input.map_or_else(|| format!("/{cmd}"), |input| format!("/{cmd} {input}"));
    ctx.defer_ephemeral().await?;
    bot.ensure_session(thread).await?;
    bot.post_user_message(thread, &prompt).await?;
    bot.submit(thread, prompt.clone()).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("queued **{prompt}**"))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

#[expect(
    clippy::unused_async,
    reason = "poise autocomplete callbacks must be async functions"
)]
async fn command_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let bot = application.data();
    let needle = partial.to_lowercase();
    let choices: Vec<AutocompleteChoice<'static>> = bot
        .session_ui(application.channel_id())
        .map(|ui| {
            ui.commands
                .iter()
                .filter(|command| {
                    command.name.to_lowercase().contains(&needle)
                        || command.description.to_lowercase().contains(&needle)
                })
                .take(super::CHOICE_LIMIT)
                .map(|command| {
                    let name = super::truncate(
                        &ui.command_hint(&command.name).map_or_else(
                            || command.name.clone(),
                            |hint| format!("{} — {hint}", command.name),
                        ),
                        100,
                    );
                    AutocompleteChoice::new(name, command.name.clone())
                })
                .collect()
        })
        .unwrap_or_default();
    CreateAutocompleteResponse::new().set_choices(choices)
}

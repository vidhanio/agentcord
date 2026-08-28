use agent_client_protocol::schema::v1::SessionModeId;
use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use crate::{Bot, BotError, BotResult};

/// Set the ACP session mode for this thread
#[poise::command(slash_command, check = "super::allowed")]
pub async fn mode(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "mode to switch to"]
    #[autocomplete = "mode_choices"]
    mode: String,
) -> Result<(), BotError> {
    let bot = (*ctx.data()).clone();
    let thread = ctx.channel_id();
    ctx.defer_ephemeral().await?;
    bot.ensure_session(thread).await?;
    let mode_id = resolve_mode(&bot, thread, &mode)?;
    bot.set_mode(thread, mode_id).await?;
    let label = bot
        .session_ui(thread)
        .and_then(|ui| ui.mode_label())
        .unwrap_or_else(|| mode.clone());
    ctx.send(
        poise::CreateReply::new()
            .content(format!("mode set to **{label}**"))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

#[expect(
    clippy::unused_async,
    reason = "poise autocomplete callbacks must be async functions"
)]
/// Suggests modes advertised by the current ACP session.
async fn mode_choices<'a>(
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
        .and_then(|ui| ui.modes)
        .map(|modes| {
            modes
                .available_modes
                .iter()
                .filter(|mode| {
                    mode.name.to_lowercase().contains(&needle)
                        || mode.id.to_string().to_lowercase().contains(&needle)
                })
                .take(super::CHOICE_LIMIT)
                .map(|mode| {
                    AutocompleteChoice::new(super::truncate(&mode.name, 100), mode.id.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    CreateAutocompleteResponse::new().set_choices(choices)
}

/// Resolves a mode by either its protocol id or display name.
fn resolve_mode(
    bot: &Bot,
    thread: serenity::all::GenericChannelId,
    mode: &str,
) -> BotResult<SessionModeId> {
    let modes = bot
        .session_ui(thread)
        .and_then(|ui| ui.modes)
        .ok_or_else(|| BotError::Other("the agent has not advertised any session modes".into()))?;
    let needle = mode.trim().to_lowercase();
    modes
        .available_modes
        .iter()
        .find(|candidate| candidate.id.to_string() == mode.trim())
        .or_else(|| {
            modes
                .available_modes
                .iter()
                .find(|candidate| candidate.name.to_lowercase() == needle)
        })
        .map(|candidate| candidate.id.clone())
        .ok_or_else(|| BotError::Other(format!("unknown mode `{mode}`")))
}

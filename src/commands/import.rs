use std::collections::HashSet;

use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse, ResolvedValue};
use tracing::warn;

use crate::{Bot, BotError, BotResult, acp::ListedSession, forum::truncate_end};

const CHOICE_LIMIT: usize = 25;
const CHOICE_NAME_LIMIT: usize = 100;

/// Import an existing ACP session from a harness into a new forum post
#[poise::command(slash_command, check = "super::allowed")]
pub async fn import(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "harness to import the session from"]
    #[autocomplete = "harness_choices"]
    harness: String,
    #[description = "session to import"]
    #[autocomplete = "session_choices"]
    session: String,
) -> Result<(), BotError> {
    let bot = (*ctx.data()).clone();
    let Some(display_name) = bot
        .config
        .agents
        .get(&harness)
        .map(|agent| agent.display_name.clone())
    else {
        return Err(BotError::Other(format!("unknown agent `{harness}`")));
    };
    ctx.defer_ephemeral().await?;
    let content = match bot.import(&harness, &session).await {
        Ok(thread) => format!(
            "imported **{display_name}** — https://discord.com/channels/{}/{}",
            bot.config.discord.guild_id, thread
        ),
        Err(error) => format!("couldn't import the session: {error}"),
    };
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

#[expect(
    clippy::unused_async,
    reason = "poise autocomplete callbacks must be async functions"
)]
async fn harness_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let bot = ctx.data();
    let needle = partial.to_lowercase();
    let choices: Vec<_> = bot
        .config
        .agents
        .iter()
        .filter(|(key, agent)| {
            key.to_lowercase().contains(&needle)
                || agent.display_name.to_lowercase().contains(&needle)
        })
        .take(CHOICE_LIMIT)
        .map(|(key, agent)| AutocompleteChoice::new(agent.display_name.clone(), key.clone()))
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}

async fn session_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let bot = application.data();
    let requested: BotResult<Vec<AutocompleteChoice<'static>>> = async {
        let harness = harness_argument(application.args)
            .ok_or_else(|| BotError::Other("choose a harness to list its sessions".into()))?;
        let listed = bot.list_sessions(harness).await?;
        let imported = bot.db.session_keys()?;
        Ok(importable_choices(harness, listed, &imported, partial))
    }
    .await;
    match requested {
        Ok(choices) => CreateAutocompleteResponse::new().set_choices(choices),
        Err(error) => {
            warn!(?error, "failed to list importable sessions");
            CreateAutocompleteResponse::new()
        }
    }
}

fn harness_argument<'a>(args: &'a [serenity::ResolvedOption<'a>]) -> Option<&'a str> {
    args.iter()
        .find(|option| option.name == "harness")
        .and_then(|option| match option.value {
            ResolvedValue::String(value) | ResolvedValue::Autocomplete { value, .. } => Some(value),
            _ => None,
        })
}

fn importable_choices(
    harness: &str,
    listed: Vec<ListedSession>,
    imported: &HashSet<(String, String)>,
    partial: &str,
) -> Vec<AutocompleteChoice<'static>> {
    let needle = partial.to_lowercase();
    listed
        .into_iter()
        .filter(|session| {
            !imported
                .iter()
                .any(|(key, id)| key == harness && *id == session.session_id)
        })
        .filter(|session| {
            session.session_id.to_lowercase().contains(&needle)
                || session
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&needle))
        })
        .take(CHOICE_LIMIT)
        .map(|session| AutocompleteChoice::new(choice_name(&session), session.session_id))
        .collect()
}

fn choice_name(session: &ListedSession) -> String {
    let fallback = format!(
        "session {}",
        session.session_id.chars().take(12).collect::<String>()
    );
    let title = session
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(fallback.as_str());
    truncate_end(title, CHOICE_NAME_LIMIT)
}

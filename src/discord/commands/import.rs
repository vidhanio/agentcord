use agent_client_protocol::schema::v1::SessionId;
use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse, ResolvedValue};
use tracing::warn;

use crate::{Bot, BotError, config::AgentKey};

/// imports an existing acp session into a new forum post.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn import(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "configured agent"] agent: usize,
    #[description = "agent-owned session id"]
    #[autocomplete = "session_choices"]
    session: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let agent_key = super::agent_key_at(&bot, agent)?;
    let session_id = SessionId::new(session.trim());
    ctx.defer_ephemeral().await?;
    let operation_bot = bot.clone();
    let operation_agent = agent_key.clone();
    let operation_session = session_id.clone();
    let operation = tokio::spawn(async move {
        operation_bot
            .import_session(&operation_agent, &operation_session)
            .await
    });
    let content = match operation.await {
        Ok(Ok(thread)) => format!(
            "imported **{}** — https://discord.com/channels/{}/{}",
            bot.config()
                .agents
                .get(&agent_key)
                .map_or_else(|| agent_key.as_ref(), |agent| agent.display_name.as_str()),
            bot.config().discord.guild_id,
            thread
        ),
        Ok(Err(error)) => {
            warn!(?error, "failed to import session");
            format!("couldn't import the session: {error}")
        }
        Err(error) => {
            warn!(?error, "session import task failed");
            format!("couldn't import the session: {error}")
        }
    };
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

/// Lists and filters sessions exposed by the selected agent.
async fn session_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let Some(agent_key) = agent_argument(&application.data(), application.args) else {
        return CreateAutocompleteResponse::new();
    };
    let sessions = match application.data().list_sessions(&agent_key).await {
        Ok(sessions) => sessions,
        Err(error) => {
            warn!(?error, agent = %agent_key, "failed to list sessions for autocomplete");
            return CreateAutocompleteResponse::new();
        }
    };
    let needle = partial.to_lowercase();
    let choices: Vec<AutocompleteChoice<'static>> = sessions
        .into_iter()
        .filter(|session| {
            session
                .session_id
                .to_string()
                .to_lowercase()
                .contains(&needle)
                || session
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&needle))
        })
        .take(super::CHOICE_LIMIT)
        .map(|session| {
            let id = session.session_id.to_string();
            let name = session
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .map_or_else(
                    || format!("session {}", super::truncate(&id, 16)),
                    |title| super::truncate(title, 100),
                );
            AutocompleteChoice::new(name, id)
        })
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}

/// Reads the selected agent from resolved slash-command arguments.
fn agent_argument(bot: &Bot, args: &[serenity::ResolvedOption<'_>]) -> Option<AgentKey> {
    args.iter()
        .find(|option| option.name == "agent")
        .and_then(|option| match option.value {
            ResolvedValue::Integer(index) => usize::try_from(index).ok(),
            _ => None,
        })
        .and_then(|index| bot.config().agents.keys().nth(index).cloned())
}

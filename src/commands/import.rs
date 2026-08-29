use agent_client_protocol::schema::v1::SessionId;
use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse, ResolvedValue};

use crate::{Bot, BotError, config::AgentKey};

/// Imports an existing ACP session into a new forum post.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn import(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "configured agent"]
    #[autocomplete = "agent_choices"]
    agent: String,
    #[description = "agent-owned session id"]
    #[autocomplete = "session_choices"]
    session: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let agent_key = AgentKey::new(agent.trim());
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
        Ok(Err(error)) => format!("couldn't import the session: {error}"),
        Err(error) => format!("couldn't import the session: {error}"),
    };
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

#[expect(
    clippy::unused_async,
    reason = "Poise autocomplete callbacks must be async"
)]
/// Suggests configured agents for the import command.
async fn agent_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let needle = partial.to_lowercase();
    let choices: Vec<AutocompleteChoice<'static>> = ctx
        .data()
        .config()
        .agents
        .iter()
        .filter(|(key, agent)| {
            key.to_lowercase().contains(&needle)
                || agent.display_name.to_lowercase().contains(&needle)
        })
        .take(super::CHOICE_LIMIT)
        .map(|(key, agent)| AutocompleteChoice::new(agent.display_name.clone(), key.to_string()))
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}

/// Lists and filters sessions exposed by the selected agent.
async fn session_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let Some(agent_key) = agent_argument(application.args) else {
        return CreateAutocompleteResponse::new();
    };
    let Ok(sessions) = application.data().list_sessions(&agent_key).await else {
        return CreateAutocompleteResponse::new();
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
fn agent_argument(args: &[serenity::ResolvedOption<'_>]) -> Option<AgentKey> {
    args.iter()
        .find(|option| option.name == "agent")
        .and_then(|option| match option.value {
            ResolvedValue::String(value) | ResolvedValue::Autocomplete { value, .. } => {
                Some(AgentKey::new(value))
            }
            _ => None,
        })
}

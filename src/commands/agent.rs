use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use crate::{Bot, BotError, config::AgentKey, projects};

/// Creates a new ACP session in a configured project.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn agent(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "configured agent"]
    #[autocomplete = "agent_choices"]
    agent: String,
    #[description = "absolute or home-relative project directory"] project: String,
    #[description = "initial prompt"] prompt: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let agent_key = AgentKey::new(agent.trim());
    let project = projects::Project::resolve(&bot.config().projects, project.trim())?;
    ctx.defer_ephemeral().await?;
    let operation_bot = bot.clone();
    let operation_agent = agent_key.clone();
    let operation = tokio::spawn(async move {
        operation_bot
            .create_session(&operation_agent, project, prompt)
            .await
    });
    let content = match operation.await {
        Ok(Ok(thread)) => format!(
            "created **{}** — https://discord.com/channels/{}/{}",
            bot.config()
                .agents
                .get(&agent_key)
                .map_or_else(|| agent_key.as_ref(), |agent| agent.display_name.as_str()),
            bot.config().discord.guild_id,
            thread
        ),
        Ok(Err(error)) => format!("couldn't create the session: {error}"),
        Err(error) => format!("couldn't create the session: {error}"),
    };
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

#[expect(
    clippy::unused_async,
    reason = "Poise autocomplete callbacks must be async"
)]
/// Suggests configured agents by key or display name.
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

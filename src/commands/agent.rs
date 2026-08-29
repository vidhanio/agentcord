use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serenity::all::{
    CreateInputText, CreateInteractionResponse, CreateLabel, CreateModal, CreateModalComponent,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse,
    InputTextStyle, LabelComponent, ModalComponent, ModalInteractionCollector,
    ModalInteractionData,
};

use crate::{Bot, BotError, config::AgentKey, projects};

/// Custom id for the agent selector in the launch modal.
const AGENT_SELECT_ID: &str = "agent";
/// Custom id for the project directory input in the launch modal.
const PROJECT_INPUT_ID: &str = "project";
/// Custom id for the initial prompt input in the launch modal.
const PROMPT_INPUT_ID: &str = "prompt";

/// Opens a modal for creating a new ACP session.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn agent(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let custom_id = ctx.interaction.id.to_string();
    let modal = build_agent_modal(&bot, &custom_id);
    ctx.interaction
        .create_response(ctx.http(), CreateInteractionResponse::Modal(modal))
        .await?;
    ctx.has_sent_initial_response.store(true, Ordering::SeqCst);

    let Some(submit) = ModalInteractionCollector::new(ctx.serenity_context())
        .filter(move |interaction| interaction.data.custom_id.as_str() == custom_id)
        .timeout(bot.config().timeouts.modal)
        .await
    else {
        return Ok(());
    };

    submit.defer_ephemeral(ctx.http()).await?;
    let values = parse_agent_modal(&submit.data);
    let Some(agent) = values.agent.filter(|value| !value.trim().is_empty()) else {
        submit
            .edit_response(
                ctx.http(),
                EditInteractionResponse::new().content("choose an agent"),
            )
            .await?;
        return Ok(());
    };
    let agent_key = AgentKey::new(agent.trim());
    if !bot.config().agents.contains_key(&agent_key) {
        submit
            .edit_response(
                ctx.http(),
                EditInteractionResponse::new().content("that agent is no longer configured"),
            )
            .await?;
        return Ok(());
    }

    let Some(project) = values.project.filter(|value| !value.trim().is_empty()) else {
        submit
            .edit_response(
                ctx.http(),
                EditInteractionResponse::new().content("the project directory is empty"),
            )
            .await?;
        return Ok(());
    };
    let project = match projects::Project::resolve(&bot.config().projects, project.trim()) {
        Ok(project) => project,
        Err(error) => {
            submit
                .edit_response(
                    ctx.http(),
                    EditInteractionResponse::new().content(error.to_string()),
                )
                .await?;
            return Ok(());
        }
    };
    let Some(prompt) = values.prompt.filter(|value| !value.trim().is_empty()) else {
        submit
            .edit_response(
                ctx.http(),
                EditInteractionResponse::new().content("the prompt is empty"),
            )
            .await?;
        return Ok(());
    };

    let operation = tokio::spawn({
        let operation_bot = bot.clone();
        let operation_agent = agent_key.clone();
        async move {
            operation_bot
                .create_session(&operation_agent, project, prompt)
                .await
        }
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
    submit
        .edit_response(ctx.http(), EditInteractionResponse::new().content(content))
        .await?;
    Ok(())
}

/// Builds the launch modal from the configured agents.
fn build_agent_modal(bot: &Bot, custom_id: &str) -> CreateModal<'static> {
    let options = bot
        .config()
        .agents
        .iter()
        .map(|(key, agent)| {
            CreateSelectMenuOption::new(agent.display_name.clone(), key.to_string())
        })
        .collect::<Vec<_>>();
    let agent_menu = CreateSelectMenu::new(
        AGENT_SELECT_ID,
        CreateSelectMenuKind::String {
            options: options.into(),
        },
    )
    .placeholder("Choose an agent")
    .min_values(1)
    .max_values(1)
    .required(true);
    let project = CreateInputText::new(InputTextStyle::Short, PROJECT_INPUT_ID)
        .placeholder("~/Projects/my-project")
        .max_length(4000)
        .required(true);
    let prompt = CreateInputText::new(InputTextStyle::Paragraph, PROMPT_INPUT_ID)
        .placeholder("What should the agent do?")
        .max_length(4000)
        .required(true);

    CreateModal::new(custom_id.to_owned(), "Create an Agentcord session").components(vec![
        CreateModalComponent::Label(CreateLabel::select_menu("Agent", agent_menu)),
        CreateModalComponent::Label(CreateLabel::input_text("Project directory", project)),
        CreateModalComponent::Label(CreateLabel::input_text("Initial prompt", prompt)),
    ])
}

/// Values submitted by the launch modal.
#[derive(Default)]
struct AgentModalValues {
    /// Selected configured agent key.
    agent: Option<String>,
    /// User-supplied project directory.
    project: Option<String>,
    /// Initial prompt sent to the new session.
    prompt: Option<String>,
}

/// Extracts the launch values from a submitted modal interaction.
fn parse_agent_modal(data: &ModalInteractionData) -> AgentModalValues {
    let mut values = AgentModalValues::default();
    for component in &data.components {
        let ModalComponent::Label(label) = component else {
            continue;
        };
        match &label.component {
            LabelComponent::SelectMenu(menu) if menu.custom_id == AGENT_SELECT_ID => {
                values.agent = menu.values.first().cloned();
            }
            LabelComponent::InputText(input) if input.custom_id == PROJECT_INPUT_ID => {
                values.project = Some(input.value.as_str().to_owned());
            }
            LabelComponent::InputText(input) if input.custom_id == PROMPT_INPUT_ID => {
                values.prompt = Some(input.value.as_str().to_owned());
            }
            _ => {}
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies modal component values are mapped to the launch form.
    #[test]
    fn parses_agent_modal_values() {
        let data: ModalInteractionData = serde_json::from_str(
            r#"
            {
              "custom_id": "launch",
              "components": [
                {"type": 18, "component": {
                  "type": 3, "custom_id": "agent", "values": ["codex"]
                }},
                {"type": 18, "component": {
                  "type": 4, "custom_id": "project", "value": "~/Projects/demo"
                }},
                {"type": 18, "component": {
                  "type": 4, "custom_id": "prompt", "value": "fix the tests"
                }}
              ]
            }
            "#,
        )
        .expect("modal payload should deserialize");

        let values = parse_agent_modal(&data);
        assert_eq!(values.agent.as_deref(), Some("codex"));
        assert_eq!(values.project.as_deref(), Some("~/Projects/demo"));
        assert_eq!(values.prompt.as_deref(), Some("fix the tests"));
    }
}

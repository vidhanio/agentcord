use poise::serenity_prelude as serenity;
use serenity::{
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateLabel,
    CreateModal, CreateModalComponent, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditInteractionResponse, InputTextStyle, LabelComponent,
    ModalComponent, ModalInteraction, ModalInteractionData, collector::ModalInteractionCollector,
};
use tracing::warn;

use crate::{Bot, BotError, BotResult};

const AGENT_SELECT_ID: &str = "agent";
const PROJECT_INPUT_ID: &str = "project";
const PROMPT_INPUT_ID: &str = "prompt";

#[poise::command(slash_command, check = "super::allowed")]
pub async fn agent(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = (*ctx.data()).clone();
    let custom_id = ctx.interaction.id.to_string();
    ctx.interaction
        .create_response(
            ctx.http(),
            CreateInteractionResponse::Modal(build_modal(&bot, &custom_id)),
        )
        .await?;

    let filter_id = custom_id.clone();
    let submit = ModalInteractionCollector::new(ctx.serenity_context())
        .filter(move |interaction| interaction.data.custom_id.as_str() == filter_id)
        .timeout(bot.config.timeouts.modal)
        .await;
    let Some(submit) = submit else {
        return Ok(());
    };
    let selection = parse_modal(&submit.data);
    let Some(agent_key) = selection.agent else {
        return reply(&submit, ctx.http(), "no agent selected").await;
    };
    let Some(project_input) = selection.project else {
        return reply(&submit, ctx.http(), "the project path is empty").await;
    };
    let prompt = selection.prompt.unwrap_or_default();
    if prompt.trim().is_empty() {
        return reply(&submit, ctx.http(), "the prompt is empty").await;
    }

    submit.defer_ephemeral(ctx.http()).await?;
    let submit = submit.clone();
    let http = ctx.serenity_context().http.clone();
    tokio::spawn(async move {
        let result = async {
            if !bot.config.agents.contains_key(&agent_key) {
                return Err(BotError::Other(format!("unknown agent `{agent_key}`")));
            }
            let project = bot.resolve_project(&project_input)?;
            let thread = bot.launch(&agent_key, project, prompt).await?;
            Ok::<_, BotError>(format!(
                "launched **{}** — https://discord.com/channels/{}/{}",
                bot.config.agents[&agent_key].display_name, bot.config.discord.guild_id, thread
            ))
        }
        .await;
        let content = result.unwrap_or_else(|error| format!("couldn't launch the agent: {error}"));
        if let Err(error) = submit
            .edit_response(&http, EditInteractionResponse::new().content(content))
            .await
        {
            warn!(?error, "failed to report `/agent` result");
        }
    });
    Ok(())
}

fn build_modal<'a>(bot: &'a Bot, custom_id: &'a str) -> CreateModal<'a> {
    let agent_options = bot
        .config
        .agents
        .iter()
        .enumerate()
        .map(|(index, (key, agent))| {
            CreateSelectMenuOption::new(&agent.display_name, key).default_selection(index == 0)
        })
        .collect();
    let agent = CreateSelectMenu::new(
        AGENT_SELECT_ID,
        CreateSelectMenuKind::String {
            options: agent_options,
        },
    );
    let project = CreateInputText::new(InputTextStyle::Short, PROJECT_INPUT_ID)
        .placeholder("/path/to/project")
        .required(true);
    let prompt = CreateInputText::new(InputTextStyle::Paragraph, PROMPT_INPUT_ID)
        .placeholder("what should the agent do?");
    CreateModal::new(custom_id, "launch an agent").components(vec![
        CreateModalComponent::Label(CreateLabel::select_menu("agent", agent)),
        CreateModalComponent::Label(CreateLabel::input_text("project", project)),
        CreateModalComponent::Label(CreateLabel::input_text("prompt", prompt)),
    ])
}

struct Selection {
    agent: Option<String>,
    project: Option<String>,
    prompt: Option<String>,
}

fn parse_modal(data: &ModalInteractionData) -> Selection {
    let mut selection = Selection {
        agent: None,
        project: None,
        prompt: None,
    };
    for component in &data.components {
        let ModalComponent::Label(label) = component else {
            continue;
        };
        match &label.component {
            LabelComponent::SelectMenu(select) if select.custom_id.as_str() == AGENT_SELECT_ID => {
                selection.agent = select.values.as_slice().first().cloned();
            }
            LabelComponent::InputText(text) if text.custom_id.as_str() == PROJECT_INPUT_ID => {
                selection.project = Some(text.value.as_str().to_owned());
            }
            LabelComponent::InputText(text) if text.custom_id.as_str() == PROMPT_INPUT_ID => {
                selection.prompt = Some(text.value.as_str().to_owned());
            }
            _ => {}
        }
    }
    selection
}

async fn reply(submit: &ModalInteraction, http: &serenity::Http, content: &str) -> BotResult {
    submit
        .create_response(
            http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(content)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

//! The `/agent` command: a native modal (harness dropdown defaulted to the
//! configured default harness, workspace dropdown, prompt input) that
//! launches an agent with the same spawn/bind/relay flow the forum launch
//! used.
//!
//! Poise's derive-based modal support only knows text inputs; the `/agent`
//! modal carries select menus, so it is built by hand: the modal is sent
//! directly on the command interaction, the submit is awaited through
//! serenity's modal collector, and the launch result edits the submit's
//! deferred response (see the note in [`agent`]).

use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateLabel,
    CreateModal, CreateModalComponent, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditInteractionResponse, InputTextStyle, LabelComponent,
    ModalComponent, ModalInteraction, ModalInteractionData, collector::ModalInteractionCollector,
};
use tracing::{info, warn};

use crate::{
    Bot, BotResult, config::DEFAULT_HARNESS, error::BotError, forum, herdr::SessionPath,
    relay::RelayJob, session::Harness,
};

/// How long the `/agent` modal waits for the user to submit it.
const MODAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Discord select menus accept at most 25 options; more workspaces than
/// that are dropped (newest last after sorting) with a warning.
const MAX_WORKSPACE_OPTIONS: usize = 25;

/// Component custom ids inside the agent modal.
const HARNESS_SELECT_ID: &str = "harness";
const WORKSPACE_SELECT_ID: &str = "workspace";
const PROMPT_INPUT_ID: &str = "prompt";

/// launch an agent in a herdr workspace via a modal.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn agent(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();

    let workspaces = bot
        .herdr
        .list_workspaces()
        .await
        .map_err(|error| BotError::Other(format!("couldn't reach herdr: {error}")))?;
    if workspaces.is_empty() {
        ctx.send(
            poise::CreateReply::new()
                .content("there are no herdr workspaces to launch into.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    let mut labels = workspaces.into_iter().map(|w| w.label).collect::<Vec<_>>();
    labels.sort();
    if labels.len() > MAX_WORKSPACE_OPTIONS {
        warn!(
            dropped = labels.len() - MAX_WORKSPACE_OPTIONS,
            "too many workspaces for the modal dropdown, dropping some"
        );
        labels.truncate(MAX_WORKSPACE_OPTIONS);
    }

    // When the command runs in a workspace's forum (or in a post inside
    // one), preselect that workspace in the dropdown.
    let default_workspace = match ctx.channel().await {
        Some(serenity::Channel::Guild(channel)) => Some(channel.id),
        Some(serenity::Channel::GuildThread(thread)) => Some(thread.parent_id),
        _ => None,
    };
    let default_workspace = match default_workspace {
        Some(id) => {
            let id = i64::try_from(id.get())
                .map_err(|_| BotError::Other("channel id overflows i64".into()))?;
            bot.db
                .workspace_by_forum(id)
                .await
                .ok()
                .flatten()
                .map(|row| row.label)
        }
        None => None,
    };

    // The modal is built by hand (poise's derive only knows text inputs):
    // sent as the command's initial response, then the submit is awaited
    // through the modal collector. The custom id is the interaction id, so
    // concurrent invocations never cross.
    let custom_id = ctx.interaction.id.to_string();
    let modal = build_agent_modal(&custom_id, &labels, default_workspace.as_deref());
    ctx.interaction
        .create_response(ctx.http(), CreateInteractionResponse::Modal(modal))
        .await?;

    let submit = ModalInteractionCollector::new(ctx.serenity_context())
        .filter(move |m| m.data.custom_id.as_str() == custom_id.as_str())
        .timeout(MODAL_TIMEOUT)
        .await;
    let Some(submit) = submit else {
        // The user never submitted; nothing to do.
        return Ok(());
    };

    let selection = parse_agent_modal(&submit.data);
    let harness = selection.harness.unwrap_or(DEFAULT_HARNESS);
    let Some(workspace_label) = selection.workspace else {
        reply_to_submit(&submit, ctx.http(), "no workspace selected.").await?;
        return Ok(());
    };
    let prompt = selection.prompt.unwrap_or_default();
    if prompt.trim().is_empty() {
        reply_to_submit(&submit, ctx.http(), "the prompt is empty.").await?;
        return Ok(());
    }

    // The launch can take a while (agent startup detection); defer the
    // submit ephemerally so its 3-second window is never missed, then edit
    // the deferred response with the outcome.
    submit.defer_ephemeral(ctx.http()).await?;
    let submit = submit.clone();
    let context = ctx.serenity_context().clone();
    tokio::spawn(async move {
        let outcome = launch_from_modal(&bot, &context, harness, &workspace_label, &prompt).await;
        let message = match outcome {
            Ok(link) => format!(
                "launched a **{}** agent in workspace `{workspace_label}` — {link}",
                harness.as_str()
            ),
            Err(error) => format!("couldn't launch the agent: {error}"),
        };
        if let Err(error) = submit
            .edit_response(
                &context.http,
                EditInteractionResponse::new().content(message),
            )
            .await
        {
            warn!(?error, "failed to report the /agent launch outcome");
        }
    });

    Ok(())
}

/// The agent modal: the harness dropdown (the configured default harness
/// preselected), the workspace dropdown (preselected when the command ran
/// in a managed forum), and the prompt input.
fn build_agent_modal<'a>(
    custom_id: &'a str,
    workspace_labels: &'a [String],
    default_workspace: Option<&str>,
) -> CreateModal<'a> {
    let harness_menu = CreateSelectMenu::new(
        HARNESS_SELECT_ID,
        CreateSelectMenuKind::String {
            options: Harness::ALL
                .iter()
                .map(|harness| {
                    CreateSelectMenuOption::new(harness.as_str(), harness.as_str())
                        .default_selection(*harness == DEFAULT_HARNESS)
                })
                .collect(),
        },
    );
    let workspace_menu = CreateSelectMenu::new(
        WORKSPACE_SELECT_ID,
        CreateSelectMenuKind::String {
            options: workspace_labels
                .iter()
                .map(|label| {
                    CreateSelectMenuOption::new(label, label)
                        .default_selection(default_workspace == Some(label.as_str()))
                })
                .collect(),
        },
    );
    let prompt = CreateInputText::new(InputTextStyle::Paragraph, PROMPT_INPUT_ID)
        .placeholder("what should the agent do?");

    CreateModal::new(custom_id, "launch an agent").components(vec![
        CreateModalComponent::Label(CreateLabel::select_menu("harness", harness_menu)),
        CreateModalComponent::Label(CreateLabel::select_menu("workspace", workspace_menu)),
        CreateModalComponent::Label(CreateLabel::input_text("prompt", prompt)),
    ])
}

/// Spawns the agent (reusing the forum's launch machinery), binds its
/// session to a forum post, relays the prompt, and returns the thread
/// link. All ephemeral reporting happens in the caller.
async fn launch_from_modal(
    bot: &Bot,
    ctx: &serenity::Context,
    harness: Harness,
    workspace_label: &str,
    prompt: &str,
) -> BotResult<String> {
    let Some(workspace) = bot.forum.workspace_by_label(workspace_label).await? else {
        return Err(BotError::Other(format!(
            "workspace `{workspace_label}` no longer exists"
        )));
    };

    let name = bot.forum.fresh_agent_name().await?;
    let cwd = bot.forum.launch_cwd(workspace_label).await;

    info!(%name, harness = harness.as_str(), %workspace_label, "/agent launches agent");

    let started = bot
        .forum
        .spawn_in_workspace(&workspace, &name, harness, &cwd, &[])
        .await?;
    if let Err(error) = bot.forum.ensure_session_post(ctx, &started).await {
        warn!(?error, %name, "failed to bind /agent session to a post");
    }

    // The agent's session reference may lag the launch; the empty path
    // resolves to no row yet, which surfaces the wait for the post.
    let session_path = started.agent_session.as_ref().map_or_else(
        || SessionPath::from(String::new()),
        |session| session.value.clone(),
    );
    let post = bot
        .db
        .get_session(&session_path)
        .await?
        .and_then(|session| session.post_channel_id)
        .ok_or_else(|| BotError::Other("session has no forum post yet".into()))?;
    let link = format!(
        "https://discord.com/channels/{}/{}",
        bot.config.guild_id, post
    );

    if !prompt.trim().is_empty() {
        bot.relay
            .submit(
                ctx.clone(),
                &started.pane_id,
                RelayJob {
                    channel_id: forum::from_i64(post)?,
                    text: prompt.to_owned(),
                },
            )
            .await?;
    }

    Ok(link)
}

/// Replies to a modal submit with an ephemeral error message.
async fn reply_to_submit(
    submit: &ModalInteraction,
    http: &serenity::Http,
    message: &str,
) -> BotResult<()> {
    submit
        .create_response(
            http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            ),
        )
        .await?;
    Ok(())
}

/// The `/agent` modal's submitted values.
#[derive(Debug, PartialEq, Eq)]
struct AgentSelection {
    /// The selected harness, when the dropdown value parsed.
    harness: Option<Harness>,
    /// The selected workspace label.
    workspace: Option<String>,
    /// The prompt text.
    prompt: Option<String>,
}

/// Extracts the harness, workspace, and prompt from a submitted modal.
fn parse_agent_modal(data: &ModalInteractionData) -> AgentSelection {
    let mut harness = None;
    let mut workspace = None;
    let mut prompt = None;
    for component in &data.components {
        let ModalComponent::Label(label) = component else {
            continue;
        };
        match &label.component {
            LabelComponent::SelectMenu(select) => match select.custom_id.as_str() {
                HARNESS_SELECT_ID => {
                    harness = select
                        .values
                        .as_slice()
                        .first()
                        .and_then(|v| Harness::parse(v));
                }
                WORKSPACE_SELECT_ID => {
                    workspace = select.values.as_slice().first().cloned();
                }
                _ => {}
            },
            LabelComponent::InputText(text) if text.custom_id.as_str() == PROMPT_INPUT_ID => {
                prompt = Some(text.value.as_str().to_owned());
            }
            _ => {}
        }
    }
    AgentSelection {
        harness,
        workspace,
        prompt,
    }
}

#[cfg(test)]
mod tests {
    use poise::serenity_prelude::ModalInteractionData;

    use super::{AgentSelection, parse_agent_modal};
    use crate::config::DEFAULT_HARNESS;

    /// A submitted modal payload in Discord's wire shape (Component V2:
    /// labels wrapping the select menus and the input). Raw string JSON —
    /// the modal model peeks `RawValue`, which `serde_json::from_value`
    /// cannot provide.
    fn modal_data(harness: &str, workspace: &str, prompt: &str) -> ModalInteractionData {
        let raw = serde_json::json!({
            "custom_id": "herdcord.agent",
            "components": [
                {"type": 18, "component": {"type": 3, "custom_id": "harness", "values": [harness]}},
                {"type": 18, "component": {"type": 3, "custom_id": "workspace", "values": [workspace]}},
                {"type": 18, "component": {"type": 4, "custom_id": "prompt", "value": prompt}}
            ]
        })
        .to_string();
        serde_json::from_str(&raw).expect("modal payload parses")
    }

    #[test]
    fn parse_agent_modal_extracts_harness_workspace_and_prompt() {
        let data = modal_data("claude-code", "my-workspace", "fix the bug");
        let selection = parse_agent_modal(&data);
        assert_eq!(selection.harness, Some(crate::session::Harness::ClaudeCode));
        assert_eq!(selection.workspace.as_deref(), Some("my-workspace"));
        assert_eq!(selection.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_agent_modal_falls_back_on_unknown_harness() {
        let data = modal_data("bogus", "my-workspace", "hi");
        let selection = parse_agent_modal(&data);
        assert_eq!(selection.harness, None);
        assert_eq!(selection.workspace.as_deref(), Some("my-workspace"));
        assert_eq!(selection.prompt.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_agent_modal_tolerates_empty_payloads() {
        let data: ModalInteractionData =
            serde_json::from_str(r#"{"custom_id": "herdcord.agent", "components": []}"#)
                .expect("empty modal parses");
        assert_eq!(
            parse_agent_modal(&data),
            AgentSelection {
                harness: None,
                workspace: None,
                prompt: None,
            }
        );
    }

    #[test]
    fn agent_modal_preselects_the_default_workspace() {
        use super::{WORKSPACE_SELECT_ID, build_agent_modal};

        let labels = ["alpha".to_owned(), "beta".to_owned()];
        let modal = build_agent_modal("custom", &labels, Some("beta"));
        let value = serde_json::to_value(&modal).expect("modal serializes");
        let workspace = value["components"]
            .as_array()
            .expect("components")
            .iter()
            .map(|component| &component["component"])
            .find(|component| component["custom_id"].as_str() == Some(WORKSPACE_SELECT_ID))
            .expect("workspace select");
        let defaults = workspace["options"]
            .as_array()
            .expect("options")
            .iter()
            .filter(|option| option["default"].as_bool() == Some(true))
            .map(|option| option["value"].as_str().expect("option value"))
            .collect::<Vec<_>>();
        assert_eq!(defaults, vec!["beta"]);
    }

    #[test]
    fn agent_modal_defaults_only_the_harness_without_a_channel_workspace() {
        use super::build_agent_modal;

        let labels = ["alpha".to_owned()];
        let modal = build_agent_modal("custom", &labels, None);
        let value = serde_json::to_value(&modal).expect("modal serializes");
        let defaults = value["components"]
            .as_array()
            .expect("components")
            .iter()
            .flat_map(|component| {
                component["component"]["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .filter(|option| option["default"].as_bool() == Some(true))
            .map(|option| option["value"].as_str().expect("option value"))
            .collect::<Vec<_>>();
        assert_eq!(defaults, vec![DEFAULT_HARNESS.as_str()]);
    }
}

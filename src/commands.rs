//! Slash commands (via poise): `/agent` and `/herdr`.
//!
//! `/agent` opens a native modal — an agent-harness dropdown (defaulted to
//! the configured default kind), a workspace dropdown, and a prompt input —
//! and launches the agent with the same spawn/bind/relay flow the forum
//! launch used. `/herdr` runs a one-shot control-plane agent in a
//! throwaway herdr session (see [`crate::control`]) and relays its
//! acknowledgment as an ephemeral reply.
//!
//! Poise's derive-based modal support only knows text inputs; the `/agent`
//! modal carries select menus, so it is built by hand: the modal is sent
//! directly on the command interaction, the submit is awaited through
//! serenity's modal collector, and the launch result edits the submit's
//! deferred response (see the note in [`agent`]).

use std::{sync::Arc, time::Duration};

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use serenity::{
    CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateLabel,
    CreateModal, CreateModalComponent, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditInteractionResponse, InputTextStyle, LabelComponent,
    ModalComponent, ModalInteraction, ModalInteractionData, collector::ModalInteractionCollector,
};
use tracing::{info, warn};

use crate::{
    Bot, BotResult,
    config::{CONTROL_SESSION_NAME, DEFAULT_AGENT_KIND},
    control,
    error::BotError,
    forum,
    herdr::SessionPath,
    relay::RelayJob,
    session::AgentKind,
};

/// How long the `/agent` modal waits for the user to submit it.
const MODAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Discord select menus accept at most 25 options; more workspaces than
/// that are dropped (newest last after sorting) with a warning.
const MAX_WORKSPACE_OPTIONS: usize = 25;

/// Component custom ids inside the agent modal.
const KIND_SELECT_ID: &str = "kind";
const WORKSPACE_SELECT_ID: &str = "workspace";
const PROMPT_INPUT_ID: &str = "prompt";

/// The poise framework for the bot: registers the guild commands and
/// dispatches interactions to them, while the bot's own event handler
/// keeps handling messages, threads, and the lifecycle.
pub struct BotFramework {
    poise: poise::Framework<Bot, BotError>,
    guild_id: serenity::GuildId,
}

/// Builds the framework over `bot`. The guild id is captured for
/// guild-only command registration (no user install needed).
pub fn framework(bot: &Bot) -> BotFramework {
    let poise_framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![agent(), herdr()],
            on_error,
            ..Default::default()
        })
        .build();
    BotFramework {
        poise: poise_framework,
        guild_id: bot.config.guild_id,
    }
}

#[serenity::async_trait]
impl serenity::Framework for BotFramework {
    async fn init(&mut self, client: &serenity::Client) {
        self.poise.init(client).await;
        if let Err(error) = poise::builtins::register_in_guild(
            &client.http,
            &self.poise.options().commands,
            self.guild_id,
        )
        .await
        {
            warn!(?error, "failed to register slash commands");
        }
    }

    async fn dispatch(&self, ctx: &serenity::Context, event: &serenity::FullEvent) {
        self.poise.dispatch(ctx, event).await;
    }
}

/// The allowed-user gate for every command: everyone when no allowed user
/// is configured, otherwise only that user.
// The `async` is required: poise wraps check fns into a BoxFuture.
#[allow(clippy::unused_async)]
async fn allowed(ctx: poise::Context<'_, Bot, BotError>) -> Result<bool, BotError> {
    Ok(ctx.data().is_allowed(ctx.author().id))
}

/// Reports command failures and the allowed-user gate to the user.
fn on_error(error: FrameworkError<'_, Bot, BotError>) -> poise::BoxFuture<'_, ()> {
    Box::pin(async move {
        match error {
            FrameworkError::Command { error, ctx, .. } => {
                let _ = ctx
                    .send(
                        CreateReply::new()
                            .content(format!("Command failed: {error}"))
                            .ephemeral(true),
                    )
                    .await;
            }
            FrameworkError::CommandCheckFailed { ctx, .. } => {
                let _ = ctx
                    .send(
                        CreateReply::new()
                            .content("You're not allowed to use this bot.")
                            .ephemeral(true),
                    )
                    .await;
            }
            other => {
                if let Err(error) = poise::builtins::on_error(other).await {
                    warn!(?error, "error while handling framework error");
                }
            }
        }
    })
}

/// `/agent`: launch an agent in a herdr workspace via a modal.
#[poise::command(slash_command, check = "allowed")]
async fn agent(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();

    let workspaces = bot
        .herdr
        .list_workspaces()
        .await
        .map_err(|error| BotError::Other(format!("Couldn't reach herdr: {error}")))?;
    if workspaces.is_empty() {
        ctx.send(
            CreateReply::new()
                .content("There are no herdr workspaces to launch into.")
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

    // The modal is built by hand (poise's derive only knows text inputs):
    // sent as the command's initial response, then the submit is awaited
    // through the modal collector. The custom id is the interaction id, so
    // concurrent invocations never cross.
    let custom_id = ctx.interaction.id.to_string();
    let modal = build_agent_modal(&custom_id, &labels);
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
    let kind = selection.kind.unwrap_or(DEFAULT_AGENT_KIND);
    let Some(workspace_label) = selection.workspace else {
        reply_to_submit(&submit, ctx.http(), "No workspace selected.").await?;
        return Ok(());
    };
    let prompt = selection.prompt.unwrap_or_default();
    if prompt.trim().is_empty() {
        reply_to_submit(&submit, ctx.http(), "The prompt is empty.").await?;
        return Ok(());
    }

    // The launch can take a while (agent startup detection); defer the
    // submit ephemerally so its 3-second window is never missed, then edit
    // the deferred response with the outcome.
    submit.defer_ephemeral(ctx.http()).await?;
    let submit = submit.clone();
    let context = ctx.serenity_context().clone();
    tokio::spawn(async move {
        let outcome = launch_from_modal(&bot, &context, kind, &workspace_label, &prompt).await;
        let message = match outcome {
            Ok(link) => format!(
                "Launched a **{}** agent in workspace `{workspace_label}` — {link}",
                kind.label()
            ),
            Err(error) => format!("Couldn't launch the agent: {error}"),
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

/// The agent modal: the harness dropdown (the configured default kind
/// preselected), the workspace dropdown, and the prompt input.
fn build_agent_modal<'a>(custom_id: &'a str, workspace_labels: &'a [String]) -> CreateModal<'a> {
    let kind_menu = CreateSelectMenu::new(
        KIND_SELECT_ID,
        CreateSelectMenuKind::String {
            options: AgentKind::ALL
                .iter()
                .map(|kind| {
                    CreateSelectMenuOption::new(kind.label(), kind.as_str())
                        .default_selection(*kind == DEFAULT_AGENT_KIND)
                })
                .collect(),
        },
    )
    .placeholder("Agent harness");
    let workspace_menu = CreateSelectMenu::new(
        WORKSPACE_SELECT_ID,
        CreateSelectMenuKind::String {
            options: workspace_labels
                .iter()
                .map(|label| CreateSelectMenuOption::new(label, label))
                .collect(),
        },
    )
    .placeholder("Workspace");
    let prompt = CreateInputText::new(InputTextStyle::Paragraph, PROMPT_INPUT_ID)
        .placeholder("What should the agent do?")
        .min_length(1)
        .max_length(4000)
        .required(true);

    CreateModal::new(custom_id, "Launch an agent").components(vec![
        CreateModalComponent::Label(CreateLabel::select_menu("Agent harness", kind_menu)),
        CreateModalComponent::Label(CreateLabel::select_menu("Workspace", workspace_menu)),
        CreateModalComponent::Label(CreateLabel::input_text("Prompt", prompt)),
    ])
}

/// Spawns the agent (reusing the forum's launch machinery), binds its
/// session to a forum post, relays the prompt, and returns the thread
/// link. All ephemeral reporting happens in the caller.
async fn launch_from_modal(
    bot: &Bot,
    ctx: &serenity::Context,
    kind: AgentKind,
    workspace_label: &str,
    prompt: &str,
) -> BotResult<String> {
    let Some(workspace) = bot.forum.workspace_by_label(workspace_label).await? else {
        return Err(BotError::Other(format!(
            "workspace `{workspace_label}` no longer exists"
        )));
    };

    let base = forum::sanitize_agent_name(workspace_label).unwrap_or_else(|| "agent".to_owned());
    let name = bot.forum.unique_agent_name(&base).await?;
    let cwd = bot.forum.launch_cwd(workspace_label).await;

    info!(%name, kind = kind.as_str(), %workspace_label, "/agent launches agent");

    let started = bot
        .forum
        .spawn_in_workspace(&workspace, &name, kind, &cwd, &[])
        .await?;
    if let Err(error) = bot.forum.ensure_session_post(ctx, &started).await {
        warn!(?error, %name, "failed to bind /agent session to a post");
    }

    // The agent's session reference may lag the launch; the empty path
    // makes the post-prompt sync a no-op until the poll picks the session
    // up.
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
                    session_path,
                    text: prompt.to_owned(),
                },
            )
            .await?;
    }

    Ok(link)
}

/// `/herdr`: run the one-shot control-plane agent and relay its
/// acknowledgment.
#[poise::command(slash_command, check = "allowed")]
async fn herdr(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "the herdr action to perform"] action: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    ctx.defer_ephemeral().await?;

    let command = ctx.interaction.clone();
    let http = Arc::clone(&ctx.serenity_context().http);
    tokio::spawn(async move {
        // One `/herdr` at a time: the throwaway session is shared.
        let _guard = bot.control_lock.lock().await;
        let reply = match control::run_control_agent(CONTROL_SESSION_NAME, &action).await {
            Ok(acknowledgment) => acknowledgment,
            Err(error) => format!("The herdr control agent failed: {error}"),
        };
        if let Err(error) = command
            .create_followup(
                &http,
                serenity::CreateInteractionResponseFollowup::new()
                    .content(reply)
                    .ephemeral(true),
            )
            .await
        {
            warn!(?error, "failed to relay the /herdr acknowledgment");
        }
    });

    Ok(())
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
    kind: Option<AgentKind>,
    /// The selected workspace label.
    workspace: Option<String>,
    /// The prompt text.
    prompt: Option<String>,
}

/// Extracts the kind, workspace, and prompt from a submitted modal.
fn parse_agent_modal(data: &ModalInteractionData) -> AgentSelection {
    let mut kind = None;
    let mut workspace = None;
    let mut prompt = None;
    for component in &data.components {
        let ModalComponent::Label(label) = component else {
            continue;
        };
        match &label.component {
            LabelComponent::SelectMenu(select) => match select.custom_id.as_str() {
                KIND_SELECT_ID => {
                    kind = select
                        .values
                        .as_slice()
                        .first()
                        .and_then(|v| AgentKind::parse(v));
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
        kind,
        workspace,
        prompt,
    }
}

#[cfg(test)]
mod tests {
    use poise::serenity_prelude::ModalInteractionData;

    use super::{AgentSelection, parse_agent_modal};
    use crate::session::AgentKind;

    /// A submitted modal payload in Discord's wire shape (Component V2:
    /// labels wrapping the select menus and the input). Raw string JSON —
    /// the modal model peeks `RawValue`, which `serde_json::from_value`
    /// cannot provide.
    fn modal_data(kind: &str, workspace: &str, prompt: &str) -> ModalInteractionData {
        let raw = serde_json::json!({
            "custom_id": "herdcord.agent",
            "components": [
                {"type": 18, "component": {"type": 3, "custom_id": "kind", "values": [kind]}},
                {"type": 18, "component": {"type": 3, "custom_id": "workspace", "values": [workspace]}},
                {"type": 18, "component": {"type": 4, "custom_id": "prompt", "value": prompt}}
            ]
        })
        .to_string();
        serde_json::from_str(&raw).expect("modal payload parses")
    }

    #[test]
    fn parse_agent_modal_extracts_kind_workspace_and_prompt() {
        let data = modal_data("claude-code", "my-workspace", "fix the bug");
        let selection = parse_agent_modal(&data);
        assert_eq!(selection.kind, Some(AgentKind::ClaudeCode));
        assert_eq!(selection.workspace.as_deref(), Some("my-workspace"));
        assert_eq!(selection.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_agent_modal_falls_back_on_unknown_kind() {
        let data = modal_data("bogus", "my-workspace", "hi");
        let selection = parse_agent_modal(&data);
        assert_eq!(selection.kind, None);
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
                kind: None,
                workspace: None,
                prompt: None,
            }
        );
    }
}

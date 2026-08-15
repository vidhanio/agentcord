//! Slash commands (via poise): `/agent` and `/workspace`.
//!
//! `/agent` opens a native modal — an agent-harness dropdown (defaulted to
//! the configured default kind), a workspace dropdown, and a prompt input —
//! and launches the agent with the same spawn/bind/relay flow the forum
//! launch used.
//!
//! Poise's derive-based modal support only knows text inputs; the `/agent`
//! modal carries select menus, so it is built by hand: the modal is sent
//! directly on the command interaction, the submit is awaited through
//! serenity's modal collector, and the launch result edits the submit's
//! deferred response (see the note in [`agent`]).
//!
//! `/workspace` creates a herdr workspace for a folder path. Its argument
//! autocompletes directory paths, expanding `~` to the home directory for
//! both completion and resolution.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use poise::{CreateReply, FrameworkError, serenity_prelude as serenity};
use serenity::{
    AutocompleteChoice, CreateAutocompleteResponse, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateLabel, CreateModal, CreateModalComponent,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse,
    InputTextStyle, LabelComponent, ModalComponent, ModalInteraction, ModalInteractionData,
    collector::ModalInteractionCollector,
};
use tracing::{info, warn};

use crate::{
    Bot, BotResult, config::DEFAULT_AGENT_KIND, error::BotError, forum, herdr::SessionPath,
    relay::RelayJob, session::AgentKind,
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
            commands: vec![agent(), workspace()],
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
    }

    async fn dispatch(&self, ctx: &serenity::Context, event: &serenity::FullEvent) {
        // Command registration needs the application id on the HTTP
        // client; the Ready payload is the first reliable source for it,
        // so the (idempotent) guild registration happens here instead of
        // `init`.
        if let serenity::FullEvent::Ready { data_about_bot, .. } = event {
            ctx.http.set_application_id(data_about_bot.application.id);
            if let Err(error) = poise::builtins::register_in_guild(
                &ctx.http,
                &self.poise.options().commands,
                self.guild_id,
            )
            .await
            {
                warn!(?error, "failed to register slash commands");
            }
        }
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
                            .content(format!("command failed: {error}"))
                            .ephemeral(true),
                    )
                    .await;
            }
            FrameworkError::CommandCheckFailed { ctx, .. } => {
                let _ = ctx
                    .send(
                        CreateReply::new()
                            .content("you're not allowed to use this bot.")
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

/// launch an agent in a herdr workspace via a modal.
#[poise::command(slash_command, check = "allowed")]
async fn agent(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();

    let workspaces = bot
        .herdr
        .list_workspaces()
        .await
        .map_err(|error| BotError::Other(format!("couldn't reach herdr: {error}")))?;
    if workspaces.is_empty() {
        ctx.send(
            CreateReply::new()
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
    let kind = selection.kind.unwrap_or(DEFAULT_AGENT_KIND);
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
        let outcome = launch_from_modal(&bot, &context, kind, &workspace_label, &prompt).await;
        let message = match outcome {
            Ok(link) => format!(
                "launched a **{}** agent in workspace `{workspace_label}` — {link}",
                kind.as_str()
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

/// create a herdr workspace for a folder.
#[poise::command(slash_command, check = "allowed")]
async fn workspace(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "folder path, e.g. `~/code/project`"]
    #[autocomplete = "autocomplete_path"]
    path: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();

    let home = dirs::home_dir()
        .ok_or_else(|| BotError::Other("couldn't resolve your home directory".into()))?;
    let cwd = resolve_folder(&path, &home).map_err(BotError::Other)?;
    let cwd = cwd.to_string_lossy().into_owned();
    let label = Path::new(&cwd)
        .file_name()
        .map_or_else(|| cwd.clone(), |name| name.to_string_lossy().into_owned());

    // The bot keys workspace rows and forums by label, so a duplicate
    // label would collide: reject it up front.
    let workspaces = bot
        .herdr
        .list_workspaces()
        .await
        .map_err(|error| BotError::Other(format!("couldn't reach herdr: {error}")))?;
    if let Some(existing) = workspaces
        .into_iter()
        .find(|workspace| workspace.label == label)
    {
        return Err(BotError::Other(format!(
            "a workspace named `{label}` already exists (`{}`)",
            existing.workspace_id
        )));
    }

    let created = bot
        .herdr
        .create_workspace_with_pane(&label, &cwd)
        .await
        .map_err(|error| BotError::Other(format!("couldn't create the workspace: {error}")))?;
    info!(workspace = %label, %cwd, "/workspace creates herdr workspace");

    ctx.send(
        CreateReply::new()
            .content(format!(
                "created workspace `{label}` ({}) at `{cwd}`",
                created.workspace.workspace_id
            ))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Autocompletes the `/workspace` path argument with directory
/// suggestions, keeping the `~` form when the input uses one.
// The `async` is required: poise's autocomplete callback type wraps the
// function in a BoxFuture.
#[allow(clippy::unused_async)]
async fn autocomplete_path<'a>(
    _ctx: poise::ApplicationContext<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let suggestions = dirs::home_dir()
        .map(|home| complete_path(partial, &home))
        .unwrap_or_default();
    CreateAutocompleteResponse::new().set_choices(
        suggestions
            .into_iter()
            .map(AutocompleteChoice::from)
            .collect::<Vec<_>>(),
    )
}

/// How many directory suggestions the `/workspace` autocomplete may show
/// (Discord's autocomplete limit).
const MAX_PATH_SUGGESTIONS: usize = 25;

/// Resolves `input` to an absolute directory path: expands a leading `~`
/// to `home` (via [`expand_home`]), canonicalizes (resolving symlinks),
/// and rejects anything that is not a directory. Returns a user-facing
/// error message on failure.
fn resolve_folder(input: &str, home: &Path) -> Result<PathBuf, String> {
    let Some(expanded) = expand_home(input, home) else {
        return Err("only `~` expands to your home directory (`~user` is not supported)".into());
    };
    let metadata = std::fs::metadata(&expanded)
        .map_err(|error| format!("`{input}` doesn't exist: {error}"))?;
    if !metadata.is_dir() {
        return Err(format!("`{input}` is not a directory"));
    }
    std::fs::canonicalize(&expanded).map_err(|error| format!("couldn't resolve `{input}`: {error}"))
}

/// Expands a leading `~` or `~/` to `home`; the empty input and other
/// `~user` forms return `None`. Relative and absolute paths pass through
/// unchanged.
fn expand_home(input: &str, home: &Path) -> Option<PathBuf> {
    if input.is_empty() {
        return None;
    }
    if input == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    if input.starts_with('~') {
        return None;
    }
    Some(PathBuf::from(input))
}

/// Directory completions for `input` as a folder path: every directory
/// under the input's parent whose name starts with the input's basename,
/// each with a trailing slash so the user can keep typing deeper. `~` and
/// `~/…` browse from `home` and keep the `~` form in the suggestions;
/// `~user` is not supported and yields nothing. Hidden entries are
/// skipped unless the input's basename starts with a dot. Sorted
/// case-insensitively, capped at [`MAX_PATH_SUGGESTIONS`].
fn complete_path(input: &str, home: &Path) -> Vec<String> {
    if input.is_empty() {
        // One obvious entry point: the home directory.
        return vec!["~/".to_owned()];
    }

    let (search_root, prefix, basename) = if let Some(rest) = input.strip_prefix("~/") {
        let (parent, basename) = split_path(Path::new(rest));
        let rel = with_slash(&parent);
        let prefix = if rel.is_empty() {
            "~/".to_owned()
        } else {
            format!("~{rel}")
        };
        (home.join(parent), prefix, basename)
    } else if input == "~" {
        (home.to_path_buf(), "~/".to_owned(), String::new())
    } else if input.starts_with('~') {
        return Vec::new();
    } else {
        let (parent, basename) = split_path(Path::new(input));
        let root = if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.clone()
        };
        (root, with_slash(&parent), basename)
    };

    let show_hidden = basename.starts_with('.');
    let mut suggestions = Vec::new();
    let Ok(entries) = std::fs::read_dir(&search_root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !show_hidden {
            continue;
        }
        if !name.starts_with(&basename) {
            continue;
        }
        // `is_dir` on the full path follows symlinks, so linked
        // directories complete too.
        if entry.path().is_dir() {
            suggestions.push(format!("{prefix}{name}/"));
        }
    }
    suggestions.sort_by_key(|s| s.to_lowercase());
    suggestions.truncate(MAX_PATH_SUGGESTIONS);
    suggestions
}

/// The parent and file-name parts of `path`: a trailing slash (or `..`)
/// makes the whole path the parent, so its children are listed, and a
/// bare relative name has an empty parent.
fn split_path(path: &Path) -> (PathBuf, String) {
    let text = path.as_os_str().to_str().unwrap_or_default();
    if text.ends_with('/') || text.ends_with("..") {
        return (path.to_path_buf(), String::new());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let basename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent, basename)
}

/// `path` as a display string with a trailing slash; empty stays empty.
fn with_slash(path: &Path) -> String {
    let text = path.display().to_string();
    if text.is_empty() || text.ends_with('/') {
        text
    } else {
        format!("{text}/")
    }
}

/// The agent modal: the harness dropdown (the configured default kind
/// preselected), the workspace dropdown (preselected when the command ran
/// in a managed forum), and the prompt input.
fn build_agent_modal<'a>(
    custom_id: &'a str,
    workspace_labels: &'a [String],
    default_workspace: Option<&str>,
) -> CreateModal<'a> {
    let kind_menu = CreateSelectMenu::new(
        KIND_SELECT_ID,
        CreateSelectMenuKind::String {
            options: AgentKind::ALL
                .iter()
                .map(|kind| {
                    CreateSelectMenuOption::new(kind.as_str(), kind.as_str())
                        .default_selection(*kind == DEFAULT_AGENT_KIND)
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
        CreateModalComponent::Label(CreateLabel::select_menu("agent harness", kind_menu)),
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
    kind: AgentKind,
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
    use std::path::{Path, PathBuf};

    use poise::serenity_prelude::ModalInteractionData;

    use super::{AgentSelection, complete_path, expand_home, parse_agent_modal, resolve_folder};
    use crate::session::AgentKind;

    /// A throwaway directory for path tests, removed on drop. Each test
    /// uses its own name, so parallel tests never collide.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdcord-workspace-tests-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
    fn agent_modal_defaults_only_the_kind_without_a_channel_workspace() {
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
        assert_eq!(defaults, vec!["omp"]);
    }

    #[test]
    fn expand_home_expands_tilde_forms() {
        let home = Path::new("/home/test-user");
        assert_eq!(expand_home("~", home), Some(home.to_path_buf()));
        assert_eq!(expand_home("~/a/b", home), Some(home.join("a/b")));
        assert_eq!(expand_home("/abs/path", home), Some("/abs/path".into()));
        assert_eq!(expand_home("rel/path", home), Some("rel/path".into()));
        assert_eq!(expand_home("", home), None);
        assert_eq!(expand_home("~user", home), None);
        assert_eq!(expand_home("~user/thing", home), None);
    }

    #[test]
    fn resolve_folder_expands_and_validates() {
        let tmp = TempDir::new("resolve");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "x").expect("file");

        let resolved =
            resolve_folder(project.to_str().expect("utf8"), tmp.path()).expect("resolves");
        assert_eq!(resolved, project);
        let resolved = resolve_folder("~/project", tmp.path()).expect("resolves");
        assert_eq!(resolved, project);

        let missing = tmp.path().join("missing");
        let error =
            resolve_folder(missing.to_str().expect("utf8"), tmp.path()).expect_err("missing path");
        assert!(error.contains("doesn't exist"), "{error}");
        let error = resolve_folder(file.to_str().expect("utf8"), tmp.path())
            .expect_err("file is not a directory");
        assert!(error.contains("not a directory"), "{error}");
        let error = resolve_folder("~other/project", tmp.path()).expect_err("~user");
        assert!(error.contains("only `~`"), "{error}");
    }

    #[test]
    fn complete_path_lists_matching_directories() {
        let tmp = TempDir::new("complete");
        std::fs::create_dir_all(tmp.path().join("foo/bar")).expect("foo/bar");
        std::fs::create_dir_all(tmp.path().join("foobar")).expect("foobar");
        std::fs::create_dir_all(tmp.path().join("alpha")).expect("alpha");
        std::fs::create_dir_all(tmp.path().join(".hidden")).expect(".hidden");
        std::fs::write(tmp.path().join("file.txt"), "x").expect("file");
        let root = tmp.path().to_str().expect("utf8");

        assert_eq!(
            complete_path(&format!("{root}/fo"), tmp.path()),
            vec![format!("{root}/foo/"), format!("{root}/foobar/")]
        );
        // The input's own basename is listed only under the parent's
        // children (trailing slash), never as a duplicate of the input.
        assert_eq!(
            complete_path(&format!("{root}/foo/"), tmp.path()),
            vec![format!("{root}/foo/bar/")]
        );
        // Files never complete; hidden entries appear once the basename
        // starts with a dot.
        assert_eq!(
            complete_path(&format!("{root}/.h"), tmp.path()),
            vec![format!("{root}/.hidden/")]
        );
        assert_eq!(
            complete_path(&format!("{root}/file"), tmp.path()),
            Vec::<String>::new()
        );
        // A nonexistent parent yields nothing.
        assert_eq!(
            complete_path(&format!("{root}/missing/"), tmp.path()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn complete_path_keeps_tilde_form() {
        let tmp = TempDir::new("tilde");
        std::fs::create_dir_all(tmp.path().join("foo")).expect("foo");
        std::fs::create_dir_all(tmp.path().join("foobar")).expect("foobar");
        std::fs::create_dir_all(tmp.path().join("alpha")).expect("alpha");
        std::fs::create_dir_all(tmp.path().join(".hidden")).expect(".hidden");
        std::fs::write(tmp.path().join("file.txt"), "x").expect("file");

        assert_eq!(
            complete_path("~/fo", tmp.path()),
            vec!["~/foo/".to_owned(), "~/foobar/".to_owned()]
        );
        assert_eq!(
            complete_path("~", tmp.path()),
            vec![
                "~/alpha/".to_owned(),
                "~/foo/".to_owned(),
                "~/foobar/".to_owned()
            ]
        );
        // The empty input suggests the home directory as the entry point.
        assert_eq!(complete_path("", tmp.path()), vec!["~/".to_owned()]);
        // `~user` is unsupported: no suggestions.
        assert_eq!(complete_path("~user", tmp.path()), Vec::<String>::new());
    }

    #[test]
    fn complete_path_caps_at_discord_limit() {
        let tmp = TempDir::new("cap");
        for index in 0..30 {
            std::fs::create_dir_all(tmp.path().join(format!("d{index}"))).expect("dir");
        }
        let suggestions = complete_path(&format!("{}/", tmp.path().display()), tmp.path());
        assert_eq!(suggestions.len(), 25);
        assert!(suggestions.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

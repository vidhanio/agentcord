use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, ConnectionTo,
    schema::{
        ProtocolVersion,
        v1::{
            AvailableCommand, AvailableCommandInput, CancelNotification, ContentBlock,
            CreateElicitationRequest, Implementation, InitializeRequest, InitializeResponse,
            ListSessionsRequest, LoadSessionRequest, NewSessionRequest, NewSessionResponse,
            PromptRequest, RequestPermissionRequest, SessionConfigId, SessionConfigKind,
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
            SessionConfigSelectOptions, SessionId, SessionInfo, SessionModeId, SessionModeState,
            SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
            SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
        },
    },
};
use serenity::all::{Context, GenericChannelId};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};
use tracing::{error, warn};

use crate::{
    Bot, BotError, BotResult,
    config::{AgentConfig, AgentKey},
    db::{SessionRow, TurnNumber},
    elicitation,
    forum::SessionMetadata,
    permission,
    projects::{self, Project},
};

/// Commands serialized by the task that owns one ACP connection.
#[derive(Debug)]
enum SessionCommand {
    /// Binds a newly created ACP session to its Discord thread and first
    /// prompt.
    Bind {
        /// Newly created Discord forum thread.
        thread: GenericChannelId,
        /// Initial user prompt to mirror and send.
        prompt: String,
    },
    /// Sends another prompt through an already bound session.
    Prompt(String),
    /// Changes the session mode and reports protocol completion.
    SetMode {
        /// Agent-advertised mode identifier.
        mode_id: SessionModeId,
        /// Completion channel returned to the Discord command.
        done: oneshot::Sender<Result<(), agent_client_protocol::Error>>,
    },
    /// Changes one advertised session configuration option.
    SetConfig {
        /// Agent-advertised configuration identifier.
        config_id: SessionConfigId,
        /// New typed option value.
        value: SessionConfigOptionValue,
        /// Completion channel returned to the Discord command.
        done: oneshot::Sender<Result<(), agent_client_protocol::Error>>,
    },
    /// Stops the session command loop and ACP subprocess.
    Shutdown,
}

/// Agent-owned session UI state that agentcord projects into Discord:
/// available commands, modes and configuration options with their current
/// values.
#[derive(Clone, Debug, Default)]
pub struct SessionUiState {
    /// Advertised modes and current selection.
    pub modes: Option<SessionModeState>,
    /// Advertised session configuration options and values.
    pub config_options: Vec<SessionConfigOption>,
    /// Slash-like commands advertised by the agent.
    pub commands: Vec<AvailableCommand>,
}

impl SessionUiState {
    /// Replaces the advertised mode state with a protocol snapshot.
    fn apply_modes(&mut self, modes: SessionModeState) {
        self.modes = Some(modes);
    }

    /// Replaces the advertised configuration-option snapshot.
    fn apply_config_options(&mut self, config_options: Vec<SessionConfigOption>) {
        self.config_options = config_options;
    }

    /// Replaces the advertised command snapshot.
    fn apply_commands(&mut self, commands: Vec<AvailableCommand>) {
        self.commands = commands;
    }

    /// Human-readable hint text for an advertised command, if any.
    #[must_use]
    pub fn command_hint(&self, name: &str) -> Option<String> {
        let command = self.commands.iter().find(|command| command.name == name)?;
        Some(
            command
                .input
                .as_ref()
                .and_then(|input| match input {
                    AvailableCommandInput::Unstructured(unstructured) => {
                        Some(unstructured.hint.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| command.description.clone()),
        )
    }

    /// Applies a current-mode update even if the full mode list is absent.
    fn apply_current_mode(&mut self, mode_id: SessionModeId) {
        match &mut self.modes {
            Some(modes) => modes.current_mode_id = mode_id,
            None => self.modes = Some(SessionModeState::new(mode_id, Vec::new())),
        }
    }

    /// Human-readable label for the current session mode.
    #[must_use]
    pub fn mode_label(&self) -> Option<String> {
        let modes = self.modes.as_ref()?;
        let current = &modes.current_mode_id;
        Some(
            modes
                .available_modes
                .iter()
                .find(|mode| &mode.id == current)
                .map_or_else(|| current.to_string(), |mode| mode.name.clone()),
        )
    }

    /// Human-readable label for the first option of a configuration category.
    #[must_use]
    pub fn config_label(&self, category: &SessionConfigOptionCategory) -> Option<String> {
        let option = self
            .config_options
            .iter()
            .find(|option| option.category.as_ref() == Some(category))?;
        match &option.kind {
            SessionConfigKind::Select(select) => {
                let current = &select.current_value;
                Some(
                    select_options(select)
                        .into_iter()
                        .find(|candidate| &candidate.value == current)
                        .map_or_else(|| current.to_string(), |candidate| candidate.name),
                )
            }
            SessionConfigKind::Boolean(boolean) => {
                Some(if boolean.current_value { "on" } else { "off" }.into())
            }
            _ => None,
        }
    }
}

/// Flattens a select option's (possibly grouped) value list.
pub fn select_options(
    select: &agent_client_protocol::schema::v1::SessionConfigSelect,
) -> Vec<agent_client_protocol::schema::v1::SessionConfigSelectOption> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.clone(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Cloneable command and UI handle for one running ACP session.
#[derive(Clone, Debug)]
struct ActiveSession {
    /// Serialized command input for the connection owner.
    commands: mpsc::UnboundedSender<SessionCommand>,
    /// Latest coherent projection state for synchronous readers.
    state: watch::Receiver<SessionState>,
}

/// Waiters sharing one in-flight session restoration.
#[derive(Debug, Default)]
struct SessionStartup {
    /// Callers waiting for the startup owner to publish a result.
    waiters: Mutex<Vec<oneshot::Sender<Result<ActiveSession, String>>>>,
}

/// Lifecycle state for one Discord thread in the session registry.
#[derive(Debug)]
enum SessionEntry {
    /// Exactly one caller owns startup while others wait.
    Starting(Arc<SessionStartup>),
    /// A live session handle is ready for commands and UI reads.
    Running(ActiveSession),
}

/// Single source of truth for starting and active sessions by Discord thread.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    /// Short-held lifecycle map; no asynchronous work occurs under this lock.
    entries: Mutex<HashMap<GenericChannelId, SessionEntry>>,
}

/// Result of consulting the singleflight session registry.
enum RegistryAction {
    /// Reuse an already running session.
    Running(ActiveSession),
    /// Become the owner of a newly inserted startup.
    Start(Arc<SessionStartup>),
    /// Await the existing startup owner.
    Wait(oneshot::Receiver<Result<ActiveSession, String>>),
}

impl SessionRegistry {
    /// Returns a running handle, startup ownership, or a startup waiter.
    fn access(&self, thread: GenericChannelId) -> RegistryAction {
        let mut entries = self.entries.lock().expect("session registry poisoned");
        let action = match entries.get_mut(&thread) {
            Some(SessionEntry::Running(active)) => RegistryAction::Running(active.clone()),
            Some(SessionEntry::Starting(startup)) => {
                let (sender, receiver) = oneshot::channel();
                startup
                    .waiters
                    .lock()
                    .expect("session startup waiters poisoned")
                    .push(sender);
                RegistryAction::Wait(receiver)
            }
            None => {
                let startup = Arc::new(SessionStartup::default());
                entries.insert(thread, SessionEntry::Starting(startup.clone()));
                RegistryAction::Start(startup)
            }
        };
        drop(entries);
        action
    }

    /// Publishes a startup only if its identity still owns the registry entry.
    fn publish(
        &self,
        thread: GenericChannelId,
        startup: &Arc<SessionStartup>,
        active: ActiveSession,
    ) -> bool {
        let mut entries = self.entries.lock().expect("session registry poisoned");
        if matches!(entries.get(&thread), Some(SessionEntry::Starting(current)) if Arc::ptr_eq(current, startup))
        {
            entries.insert(thread, SessionEntry::Running(active));
            true
        } else {
            false
        }
    }

    /// Removes a failed startup without disturbing a replacement startup.
    fn remove_starting(&self, thread: GenericChannelId, startup: &Arc<SessionStartup>) -> bool {
        let mut entries = self.entries.lock().expect("session registry poisoned");
        if matches!(entries.get(&thread), Some(SessionEntry::Starting(current)) if Arc::ptr_eq(current, startup))
        {
            entries.remove(&thread);
            true
        } else {
            false
        }
    }

    /// Removes a stopped session only if the command channel still matches.
    fn remove_running(&self, thread: GenericChannelId, active: &ActiveSession) -> bool {
        let mut entries = self.entries.lock().expect("session registry poisoned");
        if matches!(entries.get(&thread), Some(SessionEntry::Running(current)) if current.commands.same_channel(&active.commands))
        {
            entries.remove(&thread);
            true
        } else {
            false
        }
    }
}

/// Cancellation guard that prevents abandoned `Starting` entries.
struct StartupLease<'a> {
    /// Registry containing the guarded startup entry.
    registry: &'a SessionRegistry,
    /// Discord thread whose startup is guarded.
    thread: GenericChannelId,
    /// Identity token for the guarded startup.
    startup: Arc<SessionStartup>,
    /// Whether dropping the lease should cancel the startup.
    armed: bool,
}

impl<'a> StartupLease<'a> {
    /// Arms a cancellation guard for a newly owned startup.
    const fn new(
        registry: &'a SessionRegistry,
        thread: GenericChannelId,
        startup: Arc<SessionStartup>,
    ) -> Self {
        Self {
            registry,
            thread,
            startup,
            armed: true,
        }
    }

    /// Marks a startup path as having published or failed explicitly.
    const fn finish(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupLease<'_> {
    /// Removes and notifies an abandoned startup when its owner is cancelled.
    fn drop(&mut self) {
        if self.armed && self.registry.remove_starting(self.thread, &self.startup) {
            notify_startup(
                &self.startup,
                &Err("ACP session startup was cancelled".into()),
            );
        }
    }
}

/// Requested operation for a newly spawned ACP connection.
#[derive(Debug)]
enum StartMode {
    /// Create a fresh agent session for a project.
    New {
        /// Configured agent key.
        agent_key: AgentKey,
        /// Resolved project directory and label.
        project: Project,
    },
    /// Restore an existing Discord/session binding.
    Load(SessionRow),
    /// Inspect an external session before creating an archived Discord binding.
    Import {
        /// Configured agent key.
        agent_key: AgentKey,
        /// Agent-owned session identifier.
        session_id: SessionId,
    },
}

/// A session known to a harness through `session/list`, not yet imported.
#[derive(Clone, Debug)]
pub struct ListedSession {
    /// Agent-owned session identifier.
    pub session_id: SessionId,
    /// Optional agent-reported title.
    pub title: Option<String>,
    /// Optional agent-reported last-update timestamp.
    pub updated_at: Option<SessionUpdatedAt>,
}

/// Opaque agent-reported timestamp attached to a listed session.
#[nutype::nutype(derive(Debug, Clone, PartialEq, Eq, Display, AsRef, From))]
pub struct SessionUpdatedAt(String);

/// Shared completion cell for one cached `session/list` request.
type ListingCell = OnceCell<Result<Vec<ListedSession>, String>>;

/// Time-bounded cached listing for one configured agent.
#[derive(Clone, Debug)]
pub struct CachedListing {
    /// Time at which this cache entry was installed.
    listed_at: Instant,
    /// Shared fetch result for concurrent autocomplete calls.
    listing: Arc<ListingCell>,
}

/// Maximum age of an ACP session-list result.
const LISTING_TTL: Duration = Duration::from_secs(60);

/// Discord binding and current turn captured for rendering.
#[derive(Clone, Copy, Debug)]
struct BoundSession {
    /// Discord thread receiving the projection.
    thread: GenericChannelId,
    /// Persisted turn number for unkeyed streamed output.
    turn: TurnNumber,
}

/// Controls whether notifications are hidden, replayed, or live.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UpdatePhase {
    /// Record UI state but do not render notifications yet.
    #[default]
    Muted,
    /// Render load history with replay deduplication semantics.
    Replay,
    /// Render notifications as live session output.
    Live,
}

/// Coherent session state shared through a Tokio watch channel.
#[derive(Clone, Debug, Default)]
struct SessionState {
    /// Optional Discord binding and current turn.
    bound: Option<BoundSession>,
    /// Current notification projection phase.
    phase: UpdatePhase,
    /// Latest agent-advertised UI state.
    ui: SessionUiState,
}

impl SessionState {
    /// Creates initial muted state for an optional existing binding.
    fn new(thread: Option<GenericChannelId>, turn: TurnNumber) -> Self {
        Self {
            bound: thread.map(|thread| BoundSession { thread, turn }),
            phase: UpdatePhase::Muted,
            ui: SessionUiState::default(),
        }
    }

    /// Returns the Discord thread once the session has been bound.
    fn thread(&self) -> Option<GenericChannelId> {
        self.bound.map(|bound| bound.thread)
    }
}

/// A coherent snapshot of an ACP update and the session projection state that
/// applied when it arrived.
#[derive(Debug)]
pub struct RenderUpdate {
    /// Discord thread that was bound when the update arrived.
    pub thread: GenericChannelId,
    /// Turn that was current when the update arrived.
    pub turn: TurnNumber,
    /// Whether the update originated from `session/load` replay.
    pub replay: bool,
    /// UI snapshot after applying this update.
    pub ui: SessionUiState,
    /// Original ACP update to project.
    pub update: SessionUpdate,
}

/// Cloneable startup-result sender shared by connection exit paths.
type ReadySender = mpsc::UnboundedSender<Result<SessionMetadata, String>>;

/// Spawned connection waiting to become an active Discord session.
struct PendingSession {
    /// Command input retained when the session becomes active.
    commands: mpsc::UnboundedSender<SessionCommand>,
    /// Projection-state reader retained for UI snapshots.
    state: watch::Receiver<SessionState>,
    /// First initialization success or failure.
    ready: mpsc::UnboundedReceiver<Result<SessionMetadata, String>>,
    /// Supervised ACP connection task.
    task: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
}

impl Bot {
    /// Creates a fresh ACP session, Discord post, and active registry entry.
    pub async fn launch(
        &self,
        agent_key: &AgentKey,
        project: Project,
        prompt: String,
    ) -> BotResult<GenericChannelId> {
        let ctx = self.context()?.clone();
        let mut pending = self.spawn(
            ctx.clone(),
            StartMode::New {
                agent_key: agent_key.clone(),
                project,
            },
        )?;
        let metadata = self.await_ready(&mut pending).await?;
        let ui = pending.state.borrow().ui.clone();
        let row = self.create_session_post(&metadata, Some(&ui)).await?;
        if let Err(error) = self.db.insert_session(&row) {
            let _ = row.thread_id.delete(&ctx.http, None).await;
            return Err(error);
        }
        pending
            .commands
            .send(SessionCommand::Bind {
                thread: row.thread_id,
                prompt,
            })
            .map_err(|_| BotError::Other("ACP session exited before binding".into()))?;
        self.activate(row.thread_id, pending);
        Ok(row.thread_id)
    }

    /// Queues a prompt, restoring the session first when necessary.
    pub async fn submit(&self, thread: GenericChannelId, prompt: String) -> BotResult {
        let commands = self.ensure_active(thread).await?;
        commands
            .send(SessionCommand::Prompt(prompt))
            .map_err(|_| BotError::Other("ACP session is no longer active".into()))
    }

    /// Makes sure the thread's ACP session is running, restoring it if needed.
    pub async fn ensure_session(&self, thread: GenericChannelId) -> BotResult {
        self.ensure_active(thread).await.map(|_| ())
    }

    /// Sets the session mode of the ACP session bound to a thread.
    pub async fn set_mode(&self, thread: GenericChannelId, mode_id: SessionModeId) -> BotResult {
        let commands = self.ensure_active(thread).await?;
        let (done, result) = oneshot::channel();
        commands
            .send(SessionCommand::SetMode { mode_id, done })
            .map_err(|_| BotError::Other("ACP session is no longer active".into()))?;
        await_change(result, self.config.timeouts.startup).await
    }

    /// Sets one of the session's configuration options by id.
    pub async fn set_config_option(
        &self,
        thread: GenericChannelId,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> BotResult {
        let commands = self.ensure_active(thread).await?;
        let (done, result) = oneshot::channel();
        commands
            .send(SessionCommand::SetConfig {
                config_id,
                value,
                done,
            })
            .map_err(|_| BotError::Other("ACP session is no longer active".into()))?;
        await_change(result, self.config.timeouts.startup).await
    }

    /// Resolves the command channel for a thread's session, restoring the
    /// persisted session on demand exactly like an incoming prompt would.
    async fn ensure_active(
        &self,
        thread: GenericChannelId,
    ) -> BotResult<mpsc::UnboundedSender<SessionCommand>> {
        let startup = match self.sessions.access(thread) {
            RegistryAction::Running(active) => return Ok(active.commands),
            RegistryAction::Wait(receiver) => {
                return receiver
                    .await
                    .map_err(|_| BotError::Other("ACP session startup was cancelled".into()))?
                    .map(|active| active.commands)
                    .map_err(BotError::Other);
            }
            RegistryAction::Start(startup) => startup,
        };
        let mut lease = StartupLease::new(&self.sessions, thread, startup);
        let result = self.restore_session(thread).await;
        match result {
            Ok(pending) => {
                let result = self
                    .activate_started(thread, &lease.startup, pending)
                    .map(|active| active.commands);
                lease.finish();
                result
            }
            Err(error) => {
                self.fail_start(thread, &lease.startup, error.to_string());
                lease.finish();
                Err(error)
            }
        }
    }

    /// Restores one persisted session as the singleflight startup owner.
    async fn restore_session(&self, thread: GenericChannelId) -> BotResult<PendingSession> {
        let row = self
            .db
            .session(thread)?
            .ok_or_else(|| BotError::Other("this is not an Agentcord session".into()))?;
        if self.restorable_memo(&row.agent_key) == Some(false) {
            return Err(BotError::Other(
                "this ACP session cannot be restored by its agent".into(),
            ));
        }
        let ctx = self.context()?.clone();
        let mut pending = self.spawn(ctx, StartMode::Load(row.clone()))?;
        if let Err(error) = self.await_ready(&mut pending).await {
            let _ = self.set_thread_archived(thread, &row.agent_key, true).await;
            return Err(error);
        }
        self.refresh_starter(thread, &pending.state).await;
        if let Err(error) = self
            .set_thread_archived(thread, &row.agent_key, false)
            .await
        {
            warn!(
                ?error,
                ?thread,
                "failed to mark restored ACP session active"
            );
        }
        Ok(pending)
    }

    /// Re-renders a thread's starter message from the session's current UI
    /// state, best effort.
    async fn refresh_starter(
        &self,
        thread: GenericChannelId,
        state: &watch::Receiver<SessionState>,
    ) {
        let ui = state.borrow().ui.clone();
        if let Err(error) = self.update_starter(thread, &ui, None).await {
            warn!(
                ?error,
                ?thread,
                "failed to refresh the session starter message"
            );
        }
    }

    /// Eagerly restores every persisted session through the normal registry
    /// path.
    pub async fn restore_all(&self) {
        if self.context().is_err() {
            return;
        }
        let rows = match self.db.sessions() {
            Ok(rows) => rows,
            Err(error) => {
                error!(?error, "failed to enumerate persisted ACP sessions");
                return;
            }
        };
        for row in rows {
            if !self.config.agents.contains_key(&row.agent_key)
                || self.restorable_memo(&row.agent_key) == Some(false)
            {
                let _ = self
                    .set_thread_archived(row.thread_id, &row.agent_key, true)
                    .await;
                continue;
            }
            if let Err(error) = self.ensure_active(row.thread_id).await {
                warn!(session = %row.session_id, ?error, "failed to restore ACP session");
                let _ = self
                    .set_thread_archived(row.thread_id, &row.agent_key, true)
                    .await;
            }
        }
    }

    /// Creates an archived Discord binding for an externally created session.
    pub async fn import(
        &self,
        agent_key: &AgentKey,
        session_id: &SessionId,
    ) -> BotResult<GenericChannelId> {
        let ctx = self.context()?.clone();
        if let Some(row) = self.db.agent_session(agent_key, session_id)? {
            return Err(BotError::Other(format!(
                "this session is already imported as https://discord.com/channels/{}/{}/{}",
                self.config.discord.guild_id, row.thread_id, row.thread_id
            )));
        }
        let mut pending = self.spawn(
            ctx.clone(),
            StartMode::Import {
                agent_key: agent_key.clone(),
                session_id: session_id.clone(),
            },
        )?;
        let metadata = self.await_ready(&mut pending).await?;
        let ui = pending.state.borrow().ui.clone();
        let row = self.create_session_post(&metadata, Some(&ui)).await?;
        if let Err(error) = self.db.insert_session(&row) {
            let _ = row.thread_id.delete(&ctx.http, None).await;
            return Err(error);
        }
        if let Err(error) = self
            .set_thread_archived(row.thread_id, &row.agent_key, true)
            .await
        {
            warn!(?error, thread = ?row.thread_id, "failed to archive imported session");
        }
        let _ = pending.commands.send(SessionCommand::Shutdown);
        Ok(row.thread_id)
    }

    /// Lists the sessions a harness currently exposes through `session/list`.
    ///
    /// Results are cached briefly per harness so that repeated autocomplete
    /// requests reuse a single connection.
    ///
    /// # Panics
    ///
    /// Panics if the listing cache mutex is poisoned.
    pub async fn list_sessions(&self, agent_key: &AgentKey) -> BotResult<Vec<ListedSession>> {
        if !self.config.agents.contains_key(agent_key) {
            return Err(BotError::Other(format!("unknown agent `{agent_key}`")));
        }
        let listing = {
            let mut listings = self.listings.lock().expect("listing cache poisoned");
            cached_listing(&mut listings, agent_key)
        };
        let agent_key = agent_key.clone();
        let listed = listing
            .get_or_init(|| async {
                let Some(agent) = self.config.agents.get(&agent_key).cloned() else {
                    return Err("agent is no longer configured".into());
                };
                let fetched =
                    tokio::time::timeout(self.config.timeouts.startup, fetch(agent)).await;
                match fetched {
                    Ok(Ok(listed)) => Ok(listed),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("ACP agent listing timed out".into()),
                }
            })
            .await;
        listed.clone().map_err(BotError::Other)
    }

    /// Removes a deleted thread from the registry and stops its session.
    pub(super) fn forget(&self, thread: GenericChannelId) {
        let removed = self
            .sessions
            .entries
            .lock()
            .expect("session registry poisoned")
            .remove(&thread);
        match removed {
            Some(SessionEntry::Running(active)) => {
                let _ = active.commands.send(SessionCommand::Shutdown);
            }
            Some(SessionEntry::Starting(startup)) => {
                notify_startup(&startup, &Err("session thread was deleted".into()));
            }
            None => {}
        }
    }

    /// Spawns the configured executable and its supervised ACP connection task.
    fn spawn(&self, ctx: Context, mode: StartMode) -> BotResult<PendingSession> {
        let agent_key = match &mode {
            StartMode::New { agent_key, .. } | StartMode::Import { agent_key, .. } => agent_key,
            StartMode::Load(row) => &row.agent_key,
        };
        let agent = self.config.agents.get(agent_key).ok_or_else(|| {
            BotError::Config(format!("agent `{agent_key}` is no longer configured"))
        })?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(match &mode {
            StartMode::New { .. } | StartMode::Import { .. } => {
                SessionState::new(None, TurnNumber::new(0))
            }
            StartMode::Load(row) => {
                SessionState::new(Some(row.thread_id), self.db.latest_turn(row.thread_id)?)
            }
        });
        let task = tokio::spawn(run_connection(
            self.clone(),
            ctx,
            agent.clone(),
            mode,
            state_tx,
            command_rx,
            ready_tx,
        ));
        Ok(PendingSession {
            commands,
            state,
            ready,
            task,
        })
    }

    /// Waits once for ACP initialization, aborting the task on timeout.
    async fn await_ready(&self, pending: &mut PendingSession) -> BotResult<SessionMetadata> {
        match tokio::time::timeout(self.config.timeouts.startup, pending.ready.recv()).await {
            Ok(Some(result)) => result.map_err(BotError::Other),
            Ok(None) => Err(BotError::Other("ACP agent exited during startup".into())),
            Err(_) => {
                pending.task.abort();
                Err(BotError::Other("ACP agent startup timed out".into()))
            }
        }
    }

    /// Registers a newly launched session and starts its exit monitor.
    fn activate(&self, thread: GenericChannelId, pending: PendingSession) -> ActiveSession {
        let active = ActiveSession {
            commands: pending.commands.clone(),
            state: pending.state,
        };
        self.sessions
            .entries
            .lock()
            .expect("session registry poisoned")
            .insert(thread, SessionEntry::Running(active.clone()));
        self.monitor_session(thread, active.clone(), pending.task);
        active
    }

    /// Publishes a restored session only if its startup still owns the entry.
    fn activate_started(
        &self,
        thread: GenericChannelId,
        startup: &Arc<SessionStartup>,
        pending: PendingSession,
    ) -> BotResult<ActiveSession> {
        let active = ActiveSession {
            commands: pending.commands.clone(),
            state: pending.state,
        };
        let published = self.sessions.publish(thread, startup, active.clone());
        if !published {
            let _ = pending.commands.send(SessionCommand::Shutdown);
            return Err(BotError::Other(
                "session startup was cancelled before activation".into(),
            ));
        }
        notify_startup(startup, &Ok(active.clone()));
        self.monitor_session(thread, active.clone(), pending.task);
        Ok(active)
    }

    /// Removes a failed startup and gives every waiter the same error.
    fn fail_start(&self, thread: GenericChannelId, startup: &Arc<SessionStartup>, error: String) {
        self.sessions.remove_starting(thread, startup);
        notify_startup(startup, &Err(error));
    }

    /// Removes an exited session by identity and archives only that current
    /// entry.
    fn monitor_session(
        &self,
        thread: GenericChannelId,
        active: ActiveSession,
        task: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let result = task.await;
            let removed_current = manager.sessions.remove_running(thread, &active);
            if !removed_current || matches!(result, Ok(Ok(()))) {
                return;
            }
            let Ok(Some(row)) = manager.db.session(thread) else {
                return;
            };
            if let Err(error) = manager
                .set_thread_archived(thread, &row.agent_key, true)
                .await
            {
                warn!(?error, session = %row.session_id, "failed to mark ended ACP session");
            }
        });
    }

    /// Returns a snapshot handle only for a fully running session.
    fn active(&self, thread: GenericChannelId) -> Option<ActiveSession> {
        let entries = self
            .sessions
            .entries
            .lock()
            .expect("session registry poisoned");
        match entries.get(&thread) {
            Some(SessionEntry::Running(active)) => Some(active.clone()),
            Some(SessionEntry::Starting(_)) | None => None,
        }
    }

    /// Snapshot of the UI state an active session currently advertises.
    #[must_use]
    pub fn session_ui(&self, thread: GenericChannelId) -> Option<SessionUiState> {
        Some(self.active(thread)?.state.borrow().ui.clone())
    }

    /// Remembers whether an agent supports `session/load`, learned from the
    /// first `initialize` exchange with it.
    pub(crate) fn memoize_restorable(&self, agent_key: &AgentKey, restorable: bool) {
        self.restorable
            .lock()
            .expect("restorable memo poisoned")
            .insert(agent_key.clone(), restorable);
    }

    /// Reads the learned `session/load` support for an agent.
    #[must_use]
    pub(crate) fn restorable_memo(&self, agent_key: &AgentKey) -> Option<bool> {
        self.restorable
            .lock()
            .expect("restorable memo poisoned")
            .get(agent_key)
            .copied()
    }
}

/// Completes and drains every caller waiting on one session startup.
fn notify_startup(startup: &SessionStartup, result: &Result<ActiveSession, String>) {
    for waiter in startup
        .waiters
        .lock()
        .expect("session startup waiters poisoned")
        .drain(..)
    {
        let _ = waiter.send(result.clone());
    }
}

/// Abort-on-drop wrapper for the ordered session renderer.
#[derive(Debug)]
struct RenderTask(Option<tokio::task::JoinHandle<()>>);

impl RenderTask {
    /// Drains queued rendering within a bound before aborting remaining work.
    async fn finish(mut self, timeout: Duration) {
        if let Some(mut task) = self.0.take()
            && tokio::time::timeout(timeout, &mut task).await.is_err()
        {
            task.abort();
        }
    }
}

impl Drop for RenderTask {
    /// Prevents a cancelled connection task from leaving a detached renderer.
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Streams ACP session updates into the renderer, batching updates within the
/// configured edit debounce window.
fn spawn_render_task(
    bot: &Bot,
    mut updates_rx: mpsc::UnboundedReceiver<RenderUpdate>,
) -> RenderTask {
    let render_bot = bot.clone();
    let edit_debounce = bot.config.timeouts.edit_debounce;
    RenderTask(Some(tokio::spawn(async move {
        while let Some(update) = updates_rx.recv().await {
            let thread = update.thread;
            let mut updates = vec![update];
            if !edit_debounce.is_zero() {
                let deadline = tokio::time::sleep(edit_debounce);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        next = updates_rx.recv() => match next {
                            Some(next) => updates.push(next),
                            None => break,
                        },
                        () = &mut deadline => break,
                    }
                }
            }
            if let Err(error) = render_bot.render_updates(updates).await {
                warn!(?error, ?thread, "failed to render ACP update");
            }
        }
    })))
}

/// Owns one ACP subprocess, protocol connection, callbacks, and command loop.
async fn run_connection(
    bot: Bot,
    ctx: Context,
    agent: AgentConfig,
    mode: StartMode,
    state: watch::Sender<SessionState>,
    command_rx: mpsc::UnboundedReceiver<SessionCommand>,
    ready: ReadySender,
) -> Result<(), agent_client_protocol::Error> {
    let process = AcpAgent::new(
        AcpAgentConfig::new(agent.command)
            .args(agent.args)
            .envs(agent.env),
    );
    let (updates_tx, updates_rx) = mpsc::unbounded_channel::<RenderUpdate>();
    let render_task = spawn_render_task(&bot, updates_rx);
    let render_drain_timeout = bot.config.timeouts.startup;

    let notification_state = state.clone();
    let permission_state = state.subscribe();
    let elicitation_state = state.subscribe();
    let permission_ctx = ctx.clone();
    let elicitation_ctx = ctx.clone();
    let elicitation_bot = bot.clone();
    let agent_display_name = agent.display_name.clone();
    let permission_user = bot.config.discord.allowed_user_id;
    let permission_timeout = bot.config.timeouts.permission;
    let permission_approve_all = bot.config.permissions.approve_all;
    let connection_ready = ready.clone();
    let result = agent_client_protocol::Client
        .builder()
        .name("agentcord")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                notification_state.send_modify(|state| {
                    apply_ui_state(&mut state.ui, &notification.update);
                    let Some(bound) = state.bound else {
                        return;
                    };
                    if state.phase != UpdatePhase::Muted {
                        let _ = updates_tx.send(RenderUpdate {
                            thread: bound.thread,
                            turn: bound.turn,
                            replay: state.phase == UpdatePhase::Replay,
                            ui: state.ui.clone(),
                            update: notification.update.clone(),
                        });
                    }
                });
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                if permission_approve_all {
                    return responder.respond(permission::approve_all(&request));
                }
                let Some(thread) = permission_state.borrow().thread() else {
                    return responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                        ),
                    );
                };
                let ctx = permission_ctx.clone();
                connection.spawn(async move {
                    responder.respond(
                        permission::ask(ctx, thread, permission_user, permission_timeout, request)
                            .await,
                    )
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest, responder, connection| {
                let Some(thread) = elicitation_state.borrow().thread() else {
                    return responder.respond(elicitation::declined_response());
                };
                let ctx = elicitation_ctx.clone();
                let bot = elicitation_bot.clone();
                let agent_name = agent_display_name.clone();
                connection.spawn(async move {
                    responder
                        .respond(elicitation::handle(&bot, ctx, thread, &agent_name, request).await)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            let (session_id, metadata) =
                initialize_session(&bot, &connection, &mode, &state).await?;
            let _ = connection_ready.send(Ok(metadata));
            run_commands(&bot, &ctx, &state, connection, session_id, command_rx).await
        })
        .await;

    if let Err(error) = &result {
        let _ = ready.send(Err(error.to_string()));
    }
    render_task.finish(render_drain_timeout).await;
    result
}

/// Negotiates ACP v1 and advertises only implemented client capabilities.
async fn initialize(
    connection: &ConnectionTo<Agent>,
) -> Result<InitializeResponse, agent_client_protocol::Error> {
    let capabilities = agent_client_protocol::schema::v1::ClientCapabilities::default()
        .elicitation(
            agent_client_protocol::schema::v1::ElicitationCapabilities::new()
                .form(agent_client_protocol::schema::v1::ElicitationFormCapabilities::new())
                .url(agent_client_protocol::schema::v1::ElicitationUrlCapabilities::new()),
        );
    let initialized = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(Implementation::new("agentcord", env!("CARGO_PKG_VERSION")))
                .client_capabilities(capabilities),
        )
        .block_task()
        .await?;
    if initialized.protocol_version != ProtocolVersion::V1 {
        return Err(
            agent_client_protocol::Error::invalid_request().data(format!(
                "agent negotiated unsupported ACP version {}",
                initialized.protocol_version
            )),
        );
    }
    Ok(initialized)
}

/// Creates, loads, or inspects a session after protocol initialization.
///
/// Load notifications are marked as replay until `session/load` responds; ACP
/// callback ordering guarantees earlier history retains that classification.
async fn initialize_session(
    bot: &Bot,
    connection: &ConnectionTo<Agent>,
    mode: &StartMode,
    state: &watch::Sender<SessionState>,
) -> Result<(SessionId, SessionMetadata), agent_client_protocol::Error> {
    let initialized = initialize(connection).await?;
    let restorable = initialized.agent_capabilities.load_session;
    let agent_key = match mode {
        StartMode::New { agent_key, .. } | StartMode::Import { agent_key, .. } => agent_key,
        StartMode::Load(row) => &row.agent_key,
    };
    bot.memoize_restorable(agent_key, restorable);
    let record_session_state = |state: &watch::Sender<SessionState>,
                                response: &NewSessionResponse| {
        state.send_modify(|state| {
            if let Some(modes) = response.modes.clone() {
                state.ui.apply_modes(modes);
            }
            if let Some(config_options) = response.config_options.clone() {
                state.ui.apply_config_options(config_options);
            }
        });
    };

    match mode {
        StartMode::New { agent_key, project } => {
            let response = connection
                .send_request(NewSessionRequest::new(project.path.clone()))
                .block_task()
                .await?;
            record_session_state(state, &response);
            let metadata = SessionMetadata {
                agent_key: agent_key.clone(),
                project_label: project.label.clone(),
                cwd: project.path.clone(),
                session_id: response.session_id.clone(),
                title: None,
            };
            Ok((response.session_id, metadata))
        }
        StartMode::Load(row) => {
            if !restorable {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("agent no longer advertises session/load"));
            }
            let session_id = row.session_id.clone();
            // The agent streams the full conversation history before
            // responding; tag it as replay so the renderer can deduplicate.
            state.send_modify(|state| state.phase = UpdatePhase::Replay);
            let response = connection
                .send_request(LoadSessionRequest::new(
                    session_id.clone(),
                    row.project_path.clone(),
                ))
                .block_task()
                .await?;
            state.send_modify(|state| {
                state.phase = UpdatePhase::Live;
                if let Some(modes) = response.modes {
                    state.ui.apply_modes(modes);
                }
                if let Some(config_options) = response.config_options {
                    state.ui.apply_config_options(config_options);
                }
            });
            let metadata = SessionMetadata {
                agent_key: row.agent_key.clone(),
                project_label: projects::adopt(&bot.config.projects, &row.project_path).label,
                cwd: row.project_path.clone(),
                session_id: row.session_id.clone(),
                title: None,
            };
            Ok((session_id, metadata))
        }
        StartMode::Import {
            agent_key,
            session_id,
        } => {
            if !restorable {
                return Err(agent_client_protocol::Error::invalid_request().data(
                    "agent does not advertise session/load, so its sessions cannot be imported",
                ));
            }
            let listed = find_listed_session(connection, session_id).await?;
            let project = projects::adopt(&bot.config.projects, &listed.cwd);
            let metadata = SessionMetadata {
                agent_key: agent_key.clone(),
                project_label: project.label,
                cwd: project.path,
                session_id: session_id.clone(),
                title: listed.title,
            };
            Ok((session_id.clone(), metadata))
        }
    }
}

/// Follows ACP pagination until every listed session has been collected.
async fn list_all_sessions(
    connection: &ConnectionTo<Agent>,
) -> Result<Vec<SessionInfo>, agent_client_protocol::Error> {
    let mut listed = Vec::new();
    let mut cursor = None;
    loop {
        let request = ListSessionsRequest::new().cursor(cursor.take());
        let response = connection.send_request(request).block_task().await?;
        listed.extend(response.sessions);
        let Some(next) = response.next_cursor else {
            return Ok(listed);
        };
        cursor = Some(next);
    }
}

/// Finds one agent session by id, returning a protocol error when absent.
async fn find_listed_session(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
) -> Result<SessionInfo, agent_client_protocol::Error> {
    list_all_sessions(connection)
        .await?
        .into_iter()
        .find(|listed| listed.session_id == *session_id)
        .ok_or_else(|| {
            agent_client_protocol::Error::invalid_request()
                .data(format!("agent does not know session `{session_id}`"))
        })
}

/// Reuses a fresh listing cell or installs one for a new fetch.
fn cached_listing(
    listings: &mut HashMap<AgentKey, CachedListing>,
    agent_key: &AgentKey,
) -> Arc<ListingCell> {
    if let Some(cached) = listings.get(agent_key)
        && cached.listed_at.elapsed() < LISTING_TTL
    {
        return cached.listing.clone();
    }
    let cached = CachedListing {
        listed_at: Instant::now(),
        listing: Arc::new(OnceCell::new()),
    };
    listings.insert(agent_key.clone(), cached.clone());
    cached.listing
}

/// Opens a short-lived agent connection and executes `session/list`.
async fn fetch(agent: AgentConfig) -> Result<Vec<ListedSession>, agent_client_protocol::Error> {
    let process = AcpAgent::new(
        AcpAgentConfig::new(agent.command)
            .args(agent.args)
            .envs(agent.env),
    );
    agent_client_protocol::Client
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            let initialized = initialize(&connection).await?;
            if initialized
                .agent_capabilities
                .session_capabilities
                .list
                .is_none()
            {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("agent does not advertise session/list"));
            }
            Ok(list_all_sessions(&connection)
                .await?
                .into_iter()
                .map(|listed| ListedSession {
                    session_id: listed.session_id,
                    title: listed.title,
                    updated_at: listed.updated_at.map(SessionUpdatedAt::new),
                })
                .collect())
        })
        .await
}

/// Serially executes commands against one initialized ACP connection.
async fn run_commands(
    bot: &Bot,
    ctx: &Context,
    state: &watch::Sender<SessionState>,
    connection: ConnectionTo<Agent>,
    session_id: SessionId,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    while let Some(command) = commands.recv().await {
        match command {
            SessionCommand::Bind { thread, prompt } => {
                state.send_modify(|state| {
                    state.bound = Some(BoundSession {
                        thread,
                        turn: TurnNumber::new(0),
                    });
                    state.phase = UpdatePhase::Live;
                });
                if let Err(error) = bot.post_user_message(thread, &prompt).await {
                    warn!(?error, ?thread, "failed to mirror initial user message");
                }
                prompt_agent(bot, ctx, state, &connection, &session_id, prompt).await;
            }
            SessionCommand::Prompt(prompt) => {
                prompt_agent(bot, ctx, state, &connection, &session_id, prompt).await;
            }
            SessionCommand::SetMode { mode_id, done } => {
                let result = connection
                    .send_request(SetSessionModeRequest::new(session_id.clone(), mode_id))
                    .block_task()
                    .await
                    .map(|_: SetSessionModeResponse| ());
                let _ = done.send(result);
            }
            SessionCommand::SetConfig {
                config_id,
                value,
                done,
            } => {
                let result = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        config_id,
                        value,
                    ))
                    .block_task()
                    .await
                    .map(|_: SetSessionConfigOptionResponse| ());
                let _ = done.send(result);
            }
            SessionCommand::Shutdown => break,
        }
    }
    Ok(())
}

/// Reserves a turn, sends one prompt, and cancels it on timeout.
async fn prompt_agent(
    bot: &Bot,
    ctx: &Context,
    state: &watch::Sender<SessionState>,
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    prompt: String,
) {
    let Some(thread) = state.borrow().thread() else {
        return;
    };
    let turn = match bot.db.begin_turn(thread) {
        Ok(turn) => turn,
        Err(error) => {
            warn!(?error, "failed to begin persisted ACP turn");
            return;
        }
    };
    state.send_modify(|state| {
        if let Some(bound) = &mut state.bound {
            bound.turn = turn;
        }
    });
    let request = connection
        .send_request(PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::from(prompt)],
        ))
        .block_task();
    match tokio::time::timeout(bot.config.timeouts.prompt, request).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            let _ = thread
                .say(&ctx.http, format!("ACP prompt failed: {error}"))
                .await;
        }
        Err(_) => {
            let _ = connection.send_notification(CancelNotification::new(session_id.clone()));
            let _ = thread
                .say(&ctx.http, "ACP prompt timed out and was cancelled")
                .await;
        }
    }
}

/// Records UI-relevant session updates (modes, config options) so that
/// slash-command autocomplete and the starter message can use them even when
/// update streaming is still gated off.
fn apply_ui_state(ui: &mut SessionUiState, update: &SessionUpdate) {
    match update {
        SessionUpdate::AvailableCommandsUpdate(commands) => {
            ui.apply_commands(commands.available_commands.clone());
        }
        SessionUpdate::CurrentModeUpdate(mode) => {
            ui.apply_current_mode(mode.current_mode_id.clone());
        }
        SessionUpdate::ConfigOptionUpdate(config) => {
            ui.apply_config_options(config.config_options.clone());
        }
        _ => {}
    }
}

/// Waits for a queued mode or configuration change with a bounded response.
async fn await_change(
    result: oneshot::Receiver<Result<(), agent_client_protocol::Error>>,
    timeout: Duration,
) -> BotResult {
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(BotError::Other(error.to_string())),
        Ok(Err(_)) => Err(BotError::Other(
            "the ACP session exited before applying the change".into(),
        )),
        Err(_) => Err(BotError::Other(
            "the change is queued and will apply once the current turn finishes".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal active handle for registry identity tests.
    fn active_session() -> ActiveSession {
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let (_state_tx, state) = watch::channel(SessionState::default());
        ActiveSession { commands, state }
    }

    #[tokio::test]
    /// Verifies concurrent callers join one startup and receive one handle.
    async fn concurrent_registry_access_shares_one_startup() {
        let registry = SessionRegistry::default();
        let thread = GenericChannelId::new(1);
        let RegistryAction::Start(startup) = registry.access(thread) else {
            panic!("first access should own startup");
        };
        let RegistryAction::Wait(waiter) = registry.access(thread) else {
            panic!("concurrent access should wait for the existing startup");
        };
        let active = active_session();

        assert!(registry.publish(thread, &startup, active.clone()));
        notify_startup(&startup, &Ok(active.clone()));

        let resolved = waiter.await.unwrap().unwrap();
        assert!(resolved.commands.same_channel(&active.commands));
        let RegistryAction::Running(found) = registry.access(thread) else {
            panic!("published session should be running");
        };
        assert!(found.commands.same_channel(&active.commands));
    }

    #[tokio::test]
    /// Verifies cancellation releases waiters and permits a later retry.
    async fn cancelled_startup_releases_waiters_and_allows_retry() {
        let registry = SessionRegistry::default();
        let thread = GenericChannelId::new(1);
        let RegistryAction::Start(startup) = registry.access(thread) else {
            panic!("first access should own startup");
        };
        let lease = StartupLease::new(&registry, thread, startup);
        let RegistryAction::Wait(waiter) = registry.access(thread) else {
            panic!("concurrent access should wait for the existing startup");
        };

        drop(lease);

        assert!(waiter.await.unwrap().is_err());
        assert!(matches!(registry.access(thread), RegistryAction::Start(_)));
    }

    #[test]
    /// Verifies stale startup/task identities cannot disturb replacements.
    fn stale_startup_cannot_replace_or_remove_a_new_session() {
        let registry = SessionRegistry::default();
        let thread = GenericChannelId::new(1);
        let RegistryAction::Start(old_startup) = registry.access(thread) else {
            panic!("first access should own startup");
        };
        assert!(registry.remove_starting(thread, &old_startup));
        let RegistryAction::Start(new_startup) = registry.access(thread) else {
            panic!("access after failure should retry startup");
        };
        let old_active = active_session();
        let new_active = active_session();

        assert!(!registry.publish(thread, &old_startup, old_active.clone()));
        assert!(registry.publish(thread, &new_startup, new_active.clone()));
        assert!(!registry.remove_running(thread, &old_active));

        let RegistryAction::Running(found) = registry.access(thread) else {
            panic!("replacement session should remain registered");
        };
        assert!(found.commands.same_channel(&new_active.commands));
    }
}

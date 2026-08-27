use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, ConnectionTo,
    schema::{
        ProtocolVersion,
        v1::{
            CancelNotification, ContentBlock, Implementation, InitializeRequest,
            LoadSessionRequest, NewSessionRequest, PromptRequest, RequestPermissionRequest,
            SessionId, SessionNotification, SessionUpdate,
        },
    },
};
use serenity::all::{Context, CreateMessage, GenericChannelId};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

use crate::{
    Bot, BotError, BotResult,
    config::AgentConfig,
    db::{Availability, SessionRow},
    forum::SessionMetadata,
    permission,
    projects::Project,
};

#[derive(Debug)]
enum SessionCommand {
    Bind {
        thread: GenericChannelId,
        prompt: String,
    },
    Prompt(String),
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct ActiveSession {
    generation: u64,
    commands: mpsc::UnboundedSender<SessionCommand>,
}

#[derive(Debug)]
enum StartMode {
    New { agent_key: String, project: Project },
    Load(SessionRow),
}

#[derive(Debug)]
struct Binding {
    thread: AtomicU64,
    turn: AtomicU64,
    accept_updates: AtomicBool,
}

impl Binding {
    fn new(thread: Option<GenericChannelId>, turn: u64) -> Self {
        Self {
            thread: AtomicU64::new(thread.map_or(0, GenericChannelId::get)),
            turn: AtomicU64::new(turn),
            accept_updates: AtomicBool::new(false),
        }
    }

    fn thread(&self) -> Option<GenericChannelId> {
        let value = self.thread.load(Ordering::Acquire);
        (value != 0).then(|| GenericChannelId::new(value))
    }
}

type ReadySender = Arc<Mutex<Option<oneshot::Sender<Result<SessionMetadata, String>>>>>;

struct PendingSession {
    commands: mpsc::UnboundedSender<SessionCommand>,
    ready: Option<oneshot::Receiver<Result<SessionMetadata, String>>>,
    task: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
}

impl Bot {
    pub async fn launch(
        &self,
        agent_key: &str,
        project: Project,
        prompt: String,
    ) -> BotResult<GenericChannelId> {
        let ctx = self.context()?.clone();
        let mut pending = self.spawn(
            ctx.clone(),
            StartMode::New {
                agent_key: agent_key.to_owned(),
                project,
            },
        )?;
        let metadata = self.await_ready(&mut pending).await?;
        let row = self.create_session_post(&metadata).await?;
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

    pub async fn submit(&self, thread: GenericChannelId, prompt: String) -> BotResult {
        let ctx = self.context()?.clone();
        if let Some(active) = self.active(thread) {
            return active
                .commands
                .send(SessionCommand::Prompt(prompt))
                .map_err(|_| BotError::Other("ACP session is no longer active".into()));
        }
        let resume_lock = self.resume_lock(thread);
        let _resume = resume_lock.lock().await;
        if let Some(active) = self.active(thread) {
            return active
                .commands
                .send(SessionCommand::Prompt(prompt))
                .map_err(|_| BotError::Other("ACP session is no longer active".into()));
        }
        let row = self
            .db
            .session(thread)?
            .ok_or_else(|| BotError::Other("this is not an Agentcord session".into()))?;
        if !row.restorable {
            return Err(BotError::Other(
                "this ACP session cannot be restored by its agent".into(),
            ));
        }
        let mut pending = self.spawn(ctx.clone(), StartMode::Load(row.clone()))?;
        self.await_ready(&mut pending).await?;
        pending
            .commands
            .send(SessionCommand::Prompt(prompt))
            .map_err(|_| BotError::Other("restored ACP session exited before prompting".into()))?;
        self.activate(thread, pending);
        self.update_availability(&row, Availability::Active, None)
            .await
    }

    pub async fn restore_all(&self) {
        let Ok(ctx) = self.context().cloned() else {
            return;
        };
        let rows = match self.db.sessions() {
            Ok(rows) => rows,
            Err(error) => {
                error!(?error, "failed to enumerate persisted ACP sessions");
                return;
            }
        };
        for row in rows {
            if !row.restorable || !self.config.agents.contains_key(&row.agent_key) {
                let _ = self
                    .update_availability(&row, Availability::Unavailable, None)
                    .await;
                continue;
            }
            let mut pending = match self.spawn(ctx.clone(), StartMode::Load(row.clone())) {
                Ok(pending) => pending,
                Err(error) => {
                    self.mark_restore_failure(&row, &error.to_string()).await;
                    continue;
                }
            };
            match self.await_ready(&mut pending).await {
                Ok(_) => {
                    self.activate(row.thread_id, pending);
                    let _ = self
                        .update_availability(&row, Availability::Active, None)
                        .await;
                }
                Err(error) => self.mark_restore_failure(&row, &error.to_string()).await,
            }
        }
    }

    pub(super) fn forget(&self, thread: GenericChannelId) {
        let removed = self
            .active
            .lock()
            .expect("active session map poisoned")
            .remove(&thread);
        if let Some(active) = removed {
            let _ = active.commands.send(SessionCommand::Shutdown);
        }
    }

    fn spawn(&self, ctx: Context, mode: StartMode) -> BotResult<PendingSession> {
        let agent_key = match &mode {
            StartMode::New { agent_key, .. } => agent_key,
            StartMode::Load(row) => &row.agent_key,
        };
        let agent = self.config.agents.get(agent_key).ok_or_else(|| {
            BotError::Config(format!("agent `{agent_key}` is no longer configured"))
        })?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready) = oneshot::channel();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
        let binding = Arc::new(match &mode {
            StartMode::New { .. } => Binding::new(None, 0),
            StartMode::Load(row) => Binding::new(Some(row.thread_id), row.turn),
        });
        let task = tokio::spawn(run_connection(
            Arc::new(self.clone()),
            ctx,
            agent.clone(),
            mode,
            binding,
            command_rx,
            ready_tx,
        ));
        Ok(PendingSession {
            commands,
            ready: Some(ready),
            task,
        })
    }

    async fn await_ready(&self, pending: &mut PendingSession) -> BotResult<SessionMetadata> {
        let ready = pending.ready.take().expect("pending session awaited once");
        match tokio::time::timeout(self.config.timeouts.startup, ready).await {
            Ok(Ok(result)) => result.map_err(BotError::Other),
            Ok(Err(_)) => Err(BotError::Other("ACP agent exited during startup".into())),
            Err(_) => {
                pending.task.abort();
                Err(BotError::Other("ACP agent startup timed out".into()))
            }
        }
    }

    fn activate(&self, thread: GenericChannelId, pending: PendingSession) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        self.active
            .lock()
            .expect("active session map poisoned")
            .insert(
                thread,
                ActiveSession {
                    generation,
                    commands: pending.commands,
                },
            );
        let manager = self.clone();
        tokio::spawn(async move {
            let result = pending.task.await;
            let should_remove = manager
                .active
                .lock()
                .expect("active session map poisoned")
                .get(&thread)
                .is_some_and(|active| active.generation == generation);
            if should_remove {
                manager
                    .active
                    .lock()
                    .expect("active session map poisoned")
                    .remove(&thread);
            }
            let detail = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Ok(Some(row)) = manager.db.session(thread) {
                let availability = if row.restorable {
                    Availability::Restorable
                } else {
                    Availability::Unavailable
                };
                if let Err(error) = manager
                    .update_availability(&row, availability, detail.as_deref())
                    .await
                {
                    warn!(?error, session = %row.session_id, "failed to mark ended ACP session");
                }
            }
        });
    }

    fn active(&self, thread: GenericChannelId) -> Option<ActiveSession> {
        self.active
            .lock()
            .expect("active session map poisoned")
            .get(&thread)
            .cloned()
    }

    fn resume_lock(&self, thread: GenericChannelId) -> Arc<tokio::sync::Mutex<()>> {
        self.resume_locks
            .lock()
            .expect("resume lock map poisoned")
            .entry(thread)
            .or_default()
            .clone()
    }

    async fn mark_restore_failure(&self, row: &SessionRow, error: &str) {
        warn!(session = %row.session_id, %error, "failed to restore ACP session");
        let _ = self
            .update_availability(row, Availability::Restorable, Some(error))
            .await;
    }
}

async fn run_connection(
    bot: Arc<Bot>,
    ctx: Context,
    agent: AgentConfig,
    mode: StartMode,
    binding: Arc<Binding>,
    command_rx: mpsc::UnboundedReceiver<SessionCommand>,
    ready: ReadySender,
) -> Result<(), agent_client_protocol::Error> {
    let process = AcpAgent::new(
        AcpAgentConfig::new(agent.command)
            .args(agent.args)
            .envs(agent.env),
    );
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel::<SessionUpdate>();
    let render_binding = binding.clone();
    let render_bot = bot.clone();
    tokio::spawn(async move {
        while let Some(update) = updates_rx.recv().await {
            let Some(thread) = render_binding.thread() else {
                continue;
            };
            let turn = render_binding.turn.load(Ordering::Acquire);
            if let Err(error) = render_bot.render_update(thread, turn, update).await {
                warn!(?error, ?thread, "failed to render ACP update");
            }
        }
    });

    let notification_binding = binding.clone();
    let permission_binding = binding.clone();
    let permission_ctx = ctx.clone();
    let permission_user = bot.config.discord.allowed_user_id;
    let permission_timeout = bot.config.timeouts.permission;
    let connection_ready = ready.clone();
    let result = agent_client_protocol::Client
        .builder()
        .name("agentcord")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                if notification_binding.accept_updates.load(Ordering::Acquire) {
                    let _ = updates_tx.send(notification.update);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let Some(thread) = permission_binding.thread() else {
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
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            let (session_id, metadata) = initialize_session(&connection, &mode, &binding).await?;
            signal_ready(&connection_ready, Ok(metadata));
            run_commands(&bot, &ctx, &binding, connection, session_id, command_rx).await
        })
        .await;

    if let Err(error) = &result {
        signal_ready(&ready, Err(error.to_string()));
    }
    result
}

async fn initialize_session(
    connection: &ConnectionTo<Agent>,
    mode: &StartMode,
    binding: &Binding,
) -> Result<(SessionId, SessionMetadata), agent_client_protocol::Error> {
    let initialized = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(Implementation::new("agentcord", env!("CARGO_PKG_VERSION"))),
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
    let capabilities_json = serde_json::to_string(&initialized.agent_capabilities)
        .map_err(agent_client_protocol::Error::into_internal_error)?;
    let restorable = initialized.agent_capabilities.load_session;
    let protocol_version = initialized.protocol_version.to_string();

    match mode {
        StartMode::New { agent_key, project } => {
            let response = connection
                .send_request(NewSessionRequest::new(project.path.clone()))
                .block_task()
                .await?;
            let metadata = SessionMetadata {
                agent_key: agent_key.clone(),
                project_label: project.label.clone(),
                cwd: project.path.display().to_string(),
                session_id: response.session_id.to_string(),
                protocol_version,
                capabilities_json,
                restorable,
            };
            Ok((response.session_id, metadata))
        }
        StartMode::Load(row) => {
            if !restorable {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("agent no longer advertises session/load"));
            }
            let session_id = SessionId::new(row.session_id.clone());
            connection
                .send_request(LoadSessionRequest::new(
                    session_id.clone(),
                    PathBuf::from(&row.project_path),
                ))
                .block_task()
                .await?;
            let metadata = SessionMetadata {
                agent_key: row.agent_key.clone(),
                project_label: row.project_label.clone(),
                cwd: row.project_path.clone(),
                session_id: row.session_id.clone(),
                protocol_version,
                capabilities_json,
                restorable,
            };
            binding.accept_updates.store(true, Ordering::Release);
            Ok((session_id, metadata))
        }
    }
}

async fn run_commands(
    bot: &Bot,
    ctx: &Context,
    binding: &Binding,
    connection: ConnectionTo<Agent>,
    session_id: SessionId,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    while let Some(command) = commands.recv().await {
        match command {
            SessionCommand::Bind { thread, prompt } => {
                binding.thread.store(thread.get(), Ordering::Release);
                binding.accept_updates.store(true, Ordering::Release);
                for (index, chunk) in crate::render::split_message(&prompt, 1_980)
                    .into_iter()
                    .enumerate()
                {
                    let content = if index == 0 {
                        format!("**prompt**\n{chunk}")
                    } else {
                        chunk
                    };
                    let _ = thread
                        .send_message(&ctx.http, CreateMessage::new().content(content))
                        .await;
                }
                prompt_agent(bot, ctx, binding, &connection, &session_id, prompt).await;
            }
            SessionCommand::Prompt(prompt) => {
                prompt_agent(bot, ctx, binding, &connection, &session_id, prompt).await;
            }
            SessionCommand::Shutdown => break,
        }
    }
    Ok(())
}

async fn prompt_agent(
    bot: &Bot,
    ctx: &Context,
    binding: &Binding,
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    prompt: String,
) {
    let Some(thread) = binding.thread() else {
        return;
    };
    let turn = match bot.db.begin_turn(thread) {
        Ok(turn) => turn,
        Err(error) => {
            warn!(?error, "failed to begin persisted ACP turn");
            return;
        }
    };
    binding.turn.store(turn, Ordering::Release);
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

fn signal_ready(ready: &ReadySender, result: Result<SessionMetadata, String>) {
    let sender = ready.lock().expect("ready sender poisoned").take();
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
}

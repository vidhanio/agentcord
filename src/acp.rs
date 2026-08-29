//! Per-thread ACP session supervision.
//!
//! A supervisor owns only the short-lived registry of session actors. Each
//! actor owns one ACP subprocess and serializes prompts for its persisted
//! session. ACP callbacks enqueue projection events; they never perform
//! Discord I/O themselves.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo,
    schema::{
        ProtocolVersion,
        v1::{
            ContentBlock, InitializeRequest, InitializeResponse, LoadSessionRequest, PromptRequest,
            SessionId, SessionNotification,
        },
    },
};
use serenity::all::{CreateMessage, GenericChannelId};
use tokio::sync::mpsc;
use tracing::warn;

use crate::{Bot, BotError, BotResult, PromptOrigin, db::SessionRow, render::ProjectionEvent};

const COMMAND_QUEUE_CAPACITY: usize = 32;
const UPDATE_QUEUE_CAPACITY: usize = 128;
const PROMPT_CANCEL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Signal {
    triggered: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Signal {
    fn trigger(&self) {
        self.triggered.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }
}

impl Default for Signal {
    fn default() -> Self {
        Self {
            triggered: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct ProjectionState {
    updates: mpsc::Sender<ProjectionEvent>,
    current_turn: Arc<Mutex<String>>,
    replaying: Arc<Mutex<bool>>,
    fault: Arc<Signal>,
    stop: Arc<Signal>,
}

/// Registry of one actor per persisted Discord session thread.
#[derive(Debug, Default)]
pub struct Supervisor {
    actors: Arc<Mutex<HashMap<GenericChannelId, ActorEntry>>>,
}

#[derive(Debug)]
struct ActorEntry {
    row: SessionRow,
    sender: mpsc::Sender<SessionCommand>,
    stop: Arc<Signal>,
}

impl Supervisor {
    /// Queues a prompt for a persisted session, starting its actor on demand.
    pub fn prompt(
        &self,
        bot: &Bot,
        row: &SessionRow,
        text: String,
        turn_id: String,
        origin: PromptOrigin,
    ) -> BotResult {
        let thread = row.thread_id;
        let sender = self.sender(bot, row);
        let command = SessionCommand::Prompt {
            text,
            turn_id,
            origin,
        };
        sender.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                BotError::Acp("the ACP prompt queue is full".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                self.remove(thread, &sender);
                BotError::Acp("the ACP session actor has exited".into())
            }
        })?;
        Ok(())
    }

    fn sender(&self, bot: &Bot, row: &SessionRow) -> mpsc::Sender<SessionCommand> {
        let mut actors = self.actors.lock().expect("ACP actor registry poisoned");
        if let Some(entry) = actors.get(&row.thread_id)
            && entry.row == *row
        {
            return entry.sender.clone();
        }
        if let Some(entry) = actors.remove(&row.thread_id) {
            entry.stop.trigger();
        }

        let (sender, commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let actor_sender = sender.clone();
        let actor_bot = bot.clone();
        let failure_bot = actor_bot.clone();
        let stop = Arc::new(Signal::default());
        let actor_stop = Arc::clone(&stop);
        let failure_stop = Arc::clone(&stop);
        let actor_row = row.clone();
        let registry = Arc::clone(&self.actors);
        tokio::spawn(async move {
            if let Err(error) = run_actor(actor_bot, actor_row.clone(), commands, actor_stop).await
            {
                if !failure_stop.is_triggered() {
                    notify_failure(
                        &failure_bot,
                        actor_row.thread_id,
                        format!("ACP session actor stopped: {error}"),
                    )
                    .await;
                }
                warn!(?error, thread = ?actor_row.thread_id, "ACP session actor stopped");
            }
            let mut actors = registry.lock().expect("ACP actor registry poisoned");
            if actors
                .get(&actor_row.thread_id)
                .is_some_and(|current| current.sender.same_channel(&actor_sender))
            {
                actors.remove(&actor_row.thread_id);
            }
        });
        actors.insert(
            row.thread_id,
            ActorEntry {
                row: row.clone(),
                sender: sender.clone(),
                stop,
            },
        );
        sender
    }

    fn remove(&self, thread: GenericChannelId, sender: &mpsc::Sender<SessionCommand>) {
        let mut actors = self.actors.lock().expect("ACP actor registry poisoned");
        if actors
            .get(&thread)
            .is_some_and(|current| current.sender.same_channel(sender))
        {
            actors.remove(&thread);
        }
    }
}

#[derive(Debug)]
enum SessionCommand {
    Prompt {
        text: String,
        turn_id: String,
        origin: PromptOrigin,
    },
}

async fn run_actor(
    bot: Bot,
    row: SessionRow,
    commands: mpsc::Receiver<SessionCommand>,
    stop: Arc<Signal>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(agent) = bot.config().agents.get(&row.agent_key).cloned() else {
        return Err(agent_client_protocol::Error::invalid_request()
            .data(format!("agent `{}` is no longer configured", row.agent_key)));
    };
    let process = AcpAgent::new(
        AcpAgentConfig::new(agent.command)
            .args(agent.args)
            .envs(agent.env),
    );

    let (updates, mut update_receiver) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
    let projection = ProjectionState {
        updates,
        current_turn: Arc::new(Mutex::new(String::from("restore"))),
        replaying: Arc::new(Mutex::new(true)),
        fault: Arc::new(Signal::default()),
        stop,
    };
    let thread = row.thread_id;
    let render_bot = bot.clone();
    let render_task = tokio::spawn(async move {
        while let Some(event) = update_receiver.recv().await {
            if let Err(error) = render_bot.apply_projection_event(event).await {
                warn!(?error, thread = ?thread, "failed to project ACP update");
            }
        }
    });

    let projection_updates = projection.updates.clone();
    let result = connect_agent(bot, row, process, commands, projection).await;

    drop(projection_updates);
    if let Err(error) = render_task.await {
        warn!(?error, thread = ?thread, "projection task stopped unexpectedly");
    }
    result
}

async fn connect_agent(
    bot: Bot,
    row: SessionRow,
    process: AcpAgent,
    mut commands: mpsc::Receiver<SessionCommand>,
    projection: ProjectionState,
) -> Result<(), agent_client_protocol::Error> {
    let callback_updates = projection.updates.clone();
    let callback_turn = Arc::clone(&projection.current_turn);
    let callback_replay = Arc::clone(&projection.replaying);
    let callback_fault = Arc::clone(&projection.fault);
    let expected_session = row.session_id.clone();
    let thread = row.thread_id;

    Client
        .builder()
        .name("agentcord")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                if notification.session_id != expected_session {
                    warn!(
                        expected = %expected_session,
                        received = %notification.session_id,
                        thread = ?thread,
                        "ignoring ACP notification for another session"
                    );
                    return Ok(());
                }
                let turn_id = callback_turn
                    .lock()
                    .expect("ACP turn mutex poisoned")
                    .clone();
                let replay = *callback_replay.lock().expect("ACP replay mutex poisoned");
                callback_updates
                    .try_send(ProjectionEvent {
                        thread_id: thread,
                        turn_id,
                        replay,
                        update: notification.update,
                    })
                    .map_err(|error| {
                        callback_fault.trigger();
                        let message = match error {
                            mpsc::error::TrySendError::Full(_) => {
                                "the ACP projection queue is full"
                            }
                            mpsc::error::TrySendError::Closed(_) => {
                                "the ACP projection task is no longer running"
                            }
                        };
                        agent_client_protocol::Error::internal_error().data(message)
                    })
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            let initialized = stop_aware(
                &projection.stop,
                initialize(&connection, bot.config().timeouts.startup),
            )
            .await?;
            if !initialized.agent_capabilities.load_session {
                return Err(agent_client_protocol::Error::invalid_request()
                    .data("agent does not advertise session/load"));
            }
            let load = connection.send_request(LoadSessionRequest::new(
                row.session_id.clone(),
                row.project_path.clone(),
            ));
            let load_timeout = bot.config().timeouts.startup;
            let load = async move {
                tokio::time::timeout(load_timeout, load.block_task())
                    .await
                    .map_err(|_| {
                        agent_client_protocol::Error::internal_error()
                            .data("ACP session/load timed out")
                    })??;
                Ok(())
            };
            stop_aware(&projection.stop, load).await?;
            if projection.fault.is_triggered() {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("ACP projection queue overflowed during session/load"));
            }
            *projection
                .replaying
                .lock()
                .expect("ACP replay mutex poisoned") = false;
            run_commands(
                bot,
                connection,
                row.session_id,
                thread,
                &projection,
                &mut commands,
            )
            .await
        })
        .await
}

async fn stop_aware<T>(
    stop: &Signal,
    operation: impl Future<Output = Result<T, agent_client_protocol::Error>>,
) -> Result<T, agent_client_protocol::Error> {
    if stop.is_triggered() {
        return Err(
            agent_client_protocol::Error::internal_error().data("ACP session actor was stopped")
        );
    }
    tokio::select! {
        result = operation => result,
        () = stop.notified() => Err(agent_client_protocol::Error::internal_error()
            .data("ACP session actor was stopped")),
    }
}

async fn initialize(
    connection: &ConnectionTo<Agent>,
    timeout: Duration,
) -> Result<InitializeResponse, agent_client_protocol::Error> {
    let response = tokio::time::timeout(
        timeout,
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task(),
    )
    .await
    .map_err(|_| {
        agent_client_protocol::Error::internal_error().data("ACP initialize timed out")
    })??;
    if response.protocol_version != ProtocolVersion::V1 {
        return Err(agent_client_protocol::Error::invalid_request()
            .data("agent negotiated an unsupported ACP protocol version"));
    }
    Ok(response)
}

async fn run_commands(
    bot: Bot,
    connection: ConnectionTo<Agent>,
    session_id: SessionId,
    thread: GenericChannelId,
    projection: &ProjectionState,
    commands: &mut mpsc::Receiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    loop {
        if projection.fault.is_triggered() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("ACP projection queue overflowed"));
        }
        if projection.stop.is_triggered() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("ACP session actor was stopped"));
        }
        let command = tokio::select! {
            () = projection.fault.notified() => {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("ACP projection queue overflowed"));
            }
            () = projection.stop.notified() => {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("ACP session actor was stopped"));
            }
            command = commands.recv() => command,
        };
        let Some(SessionCommand::Prompt {
            text,
            turn_id,
            origin,
        }) = command
        else {
            break;
        };
        *projection
            .current_turn
            .lock()
            .expect("ACP turn mutex poisoned") = turn_id;
        if origin == PromptOrigin::NeedsMirror
            && let Err(error) = bot.mirror_user_message(thread, &text).await
        {
            warn!(?error, thread = ?thread, "failed to mirror prompt; forwarding it anyway");
        }

        match run_prompt(
            &bot,
            &connection,
            session_id.clone(),
            thread,
            text,
            &projection.stop,
        )
        .await
        {
            Ok(_) => {}
            Err(PromptFailure::Prompt(error)) => {
                let error = BotError::Acp(error.to_string());
                notify_failure(&bot, thread, format!("ACP prompt failed: {error}")).await;
                warn!(?error, thread = ?thread, "ACP prompt failed");
            }
            Err(PromptFailure::Connection(error)) => {
                return Err(error);
            }
        }
        if projection.fault.is_triggered() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("ACP projection queue overflowed"));
        }
    }
    if projection.fault.is_triggered() {
        return Err(
            agent_client_protocol::Error::internal_error().data("ACP projection queue overflowed")
        );
    }
    Ok(())
}

async fn run_prompt(
    bot: &Bot,
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    thread: GenericChannelId,
    text: String,
    stop: &Signal,
) -> Result<agent_client_protocol::schema::v1::PromptResponse, PromptFailure> {
    let request = connection.send_request(PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::from(text)],
    ));
    let mut request_task = tokio::spawn(async move { request.block_task().await });
    tokio::select! {
        result = tokio::time::timeout(bot.config().timeouts.prompt, &mut request_task) => {
            if let Ok(result) = result {
                return result
                    .map_err(|error| {
                        PromptFailure::Connection(
                            agent_client_protocol::Error::internal_error()
                                .data(format!("ACP prompt task failed: {error}")),
                        )
                    })
                    .and_then(|result| result.map_err(PromptFailure::Prompt));
            }
        }
        () = stop.notified() => {
            let _ = connection.send_notification(
                agent_client_protocol::schema::v1::CancelNotification::new(session_id.clone()),
            );
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                    .data("ACP session actor was stopped"),
            ));
        }
    }

    if let Err(error) = connection.send_notification(
        agent_client_protocol::schema::v1::CancelNotification::new(session_id),
    ) {
        request_task.abort();
        return Err(PromptFailure::Connection(
            agent_client_protocol::Error::internal_error()
                .data(format!("failed to send ACP session/cancel: {error}")),
        ));
    }
    tokio::select! {
        result = tokio::time::timeout(PROMPT_CANCEL_GRACE, &mut request_task) => {
            if let Ok(result) = result {
                return result
                    .map_err(|error| {
                        PromptFailure::Connection(
                            agent_client_protocol::Error::internal_error()
                                .data(format!("ACP cancelled prompt task failed: {error}")),
                        )
                    })
                    .and_then(|result| result.map_err(PromptFailure::Prompt));
            }
        }
        () = stop.notified() => {
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                    .data("ACP session actor was stopped"),
            ));
        }
    }
    request_task.abort();
    Err(PromptFailure::Connection(
        agent_client_protocol::Error::internal_error().data(format!(
            "ACP prompt did not finish after session/cancel (thread {thread})"
        )),
    ))
}

#[derive(Debug)]
enum PromptFailure {
    Prompt(agent_client_protocol::Error),
    Connection(agent_client_protocol::Error),
}

async fn notify_failure(bot: &Bot, thread: GenericChannelId, message: String) {
    let Ok(context) = bot.context() else {
        return;
    };
    let _ = thread
        .send_message(&context.http, CreateMessage::new().content(message))
        .await;
}

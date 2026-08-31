//! Actor registry and commands sent to active ACP sessions.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agent_client_protocol::schema::v1::SessionConfigOption;
use serenity::all::GenericChannelId;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::{
    actor,
    model::{ModelSpec, SessionUiState},
    protocol::{NewSession, NewSessionConnection},
    runtime::Signal,
};
use crate::{Bot, BotError, BotResult, PromptOrigin, db::SessionRow};

/// Command queue capacity for one active session actor.
pub(super) const COMMAND_QUEUE_CAPACITY: usize = 32;

/// Registry of one actor per persisted Discord session thread.
#[derive(Debug, Default, Clone)]
pub(super) struct ActorRegistry {
    /// Shared map of thread IDs to active actor entries.
    inner: Arc<Mutex<HashMap<GenericChannelId, ActorEntry>>>,
}

impl ActorRegistry {
    /// Locks the registry and converts poisoning into the process invariant.
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GenericChannelId, ActorEntry>> {
        self.inner.lock().expect("acp actor registry poisoned")
    }
}

/// One active actor and the immutable session identity it serves.
#[derive(Debug)]
pub(super) struct ActorEntry {
    /// Persisted identity used to reject stale actor reuse.
    pub(super) row: SessionRow,
    /// Bounded command queue consumed by the actor.
    pub(super) sender: mpsc::Sender<SessionCommand>,
    /// Signal used to stop this actor when its row changes.
    pub(super) stop: Arc<Signal>,
    /// Completes when the actor has released its ACP connection.
    pub(super) done: oneshot::Receiver<()>,
    /// Cached session configuration used by command autocomplete.
    pub(super) ui: Arc<Mutex<SessionUiState>>,
}

/// Command delivered to a session actor.
#[derive(Debug)]
pub(super) enum SessionCommand {
    /// Sends one user prompt after optionally mirroring it to Discord.
    Prompt {
        /// Prompt text sent to ACP.
        text: String,
        /// Identifier used for unkeyed streamed output.
        turn_id: String,
        /// Whether the prompt is already visible in Discord.
        origin: PromptOrigin,
    },
    /// Changes the ACP model and optional reasoning level after the session is
    /// ready.
    SetModel {
        /// User-selected model and reasoning level.
        model: ModelSpec,
        /// Receives the ACP request result.
        done: oneshot::Sender<
            Result<
                Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
                agent_client_protocol::Error,
            >,
        >,
    },
}

/// Describes how an actor should acquire its ACP session.
pub(super) enum ActorStartup {
    /// Uses the live connection that created a new session.
    New(NewSessionConnection),
    /// Starts a connection and restores the persisted session through ACP.
    Restore,
}

/// Session actor supervisor and its active actor registry.
#[derive(Debug, Default)]
pub struct Supervisor {
    /// One active actor entry per Discord thread.
    pub(super) actors: ActorRegistry,
}

impl Supervisor {
    /// Locks the actor registry for one short synchronous mutation.
    fn actors(&self) -> std::sync::MutexGuard<'_, HashMap<GenericChannelId, ActorEntry>> {
        self.actors.lock()
    }

    /// Queues a prompt for a persisted session, starting its actor on demand.
    pub fn prompt(
        &self,
        bot: &Bot,
        row: &SessionRow,
        text: String,
        turn_id: String,
        origin: PromptOrigin,
    ) -> BotResult {
        let sender = self.sender(bot, row);
        sender
            .try_send(SessionCommand::Prompt {
                text,
                turn_id,
                origin,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => BotError::AcpQueueFull,
                mpsc::error::TrySendError::Closed(_) => {
                    self.remove(row.thread_id, &sender);
                    BotError::AcpActorExited
                }
            })?;
        Ok(())
    }

    /// Starts the actor for a persisted session without enqueueing a prompt.
    pub fn start(&self, bot: &Bot, row: &SessionRow, config_options: Vec<SessionConfigOption>) {
        drop(self.sender_with_startup(bot, row, config_options, ActorStartup::Restore));
    }

    /// Starts the actor using the live connection that created a new session.
    pub fn start_new(&self, bot: &Bot, row: &SessionRow, session: NewSession) {
        drop(self.sender_with_startup(
            bot,
            row,
            session.config_options,
            ActorStartup::New(session.connection),
        ));
    }

    /// Stops the current actor, waits for its connection to close, and starts
    /// a fresh actor after the persisted session has been validated.
    pub async fn reload(&self, bot: &Bot, row: &SessionRow) -> BotResult {
        let entry = {
            let mut actors = self.actors();
            actors.remove(&row.thread_id)
        };
        if let Some(entry) = entry {
            entry.stop.trigger();
            tokio::time::timeout(bot.config().timeouts.startup, entry.done)
                .await
                .map_err(|_| BotError::AcpReloadTimedOut)?
                .map_err(|_| BotError::AcpActorExited)?;
        }
        self.validate_session(bot, &row.agent_key, &row.session_id, &row.project_path)
            .await?;
        self.start(bot, row, Vec::new());
        Ok(())
    }

    /// Changes the model and optional reasoning level of a persisted session.
    pub async fn set_model(&self, bot: &Bot, row: &SessionRow, model: ModelSpec) -> BotResult {
        let sender = self.sender(bot, row);
        let (done, result) = oneshot::channel();
        sender
            .try_send(SessionCommand::SetModel { model, done })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => BotError::AcpQueueFull,
                mpsc::error::TrySendError::Closed(_) => {
                    self.remove(row.thread_id, &sender);
                    BotError::AcpActorExited
                }
            })?;
        match tokio::time::timeout(bot.config().timeouts.startup, result).await {
            Ok(Ok(Ok(_))) => Ok(()),
            Ok(Ok(Err(error))) => Err(super::acp_error(&error)),
            Ok(Err(_)) => Err(BotError::AcpActorExited),
            Err(_) => Err(BotError::AcpModelSelectionTimedOut),
        }
    }

    /// Returns an existing matching actor or starts one for this session row.
    fn sender(&self, bot: &Bot, row: &SessionRow) -> mpsc::Sender<SessionCommand> {
        self.sender_with_startup(bot, row, Vec::new(), ActorStartup::Restore)
    }

    /// Returns an existing matching actor or starts one with the requested
    /// connection startup mode and initial config snapshot.
    fn sender_with_startup(
        &self,
        bot: &Bot,
        row: &SessionRow,
        config_options: Vec<SessionConfigOption>,
        startup: ActorStartup,
    ) -> mpsc::Sender<SessionCommand> {
        let mut actors = self.actors();
        if let Some(entry) = actors.get(&row.thread_id)
            && same_session(&entry.row, row)
        {
            return entry.sender.clone();
        }
        if let Some(entry) = actors.remove(&row.thread_id) {
            entry.stop.trigger();
        }

        let (sender, commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let actor_sender = sender.clone();
        let actor_bot = bot.clone();
        let failure_bot = bot.clone();
        let stop = Arc::new(Signal::default());
        let actor_stop = Arc::clone(&stop);
        let failure_stop = Arc::clone(&stop);
        let actor_row = row.clone();
        let ui = Arc::new(Mutex::new(SessionUiState { config_options }));
        let actor_ui = Arc::clone(&ui);
        let registry = self.actors.clone();
        let (done_sender, done_receiver) = oneshot::channel();
        tokio::spawn(async move {
            if let Err(error) = actor::run(
                actor_bot,
                actor_row.clone(),
                commands,
                actor_stop,
                actor_ui,
                startup,
            )
            .await
            {
                if !failure_stop.is_triggered() {
                    super::prompt::notify_failure(
                        &failure_bot,
                        actor_row.thread_id,
                        format!("acp session actor stopped: {error}"),
                    )
                    .await;
                }
                warn!(?error, thread = ?actor_row.thread_id, "acp session actor stopped");
            }
            let _ = done_sender.send(());
            let mut actors = registry.lock();
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
                done: done_receiver,
                ui,
            },
        );
        sender
    }

    /// Removes an actor only if the sender still identifies the current entry.
    fn remove(&self, thread: GenericChannelId, sender: &mpsc::Sender<SessionCommand>) {
        let mut actors = self.actors();
        if actors
            .get(&thread)
            .is_some_and(|current| current.sender.same_channel(sender))
        {
            actors.remove(&thread);
        }
    }

    /// Returns a snapshot of an active session's advertised configuration.
    pub fn session_ui(&self, thread: GenericChannelId) -> Option<SessionUiState> {
        let actors = self.actors();
        let ui = actors.get(&thread)?.ui.clone();
        drop(actors);
        Some(ui.lock().expect("acp session ui mutex poisoned").clone())
    }

    /// Stops and removes the actor for a deleted session thread.
    pub fn stop(&self, thread: GenericChannelId) {
        let entry = self.actors().remove(&thread);
        if let Some(entry) = entry {
            entry.stop.trigger();
        }
    }
}

/// Compares the immutable identity fields used by an actor connection.
fn same_session(left: &SessionRow, right: &SessionRow) -> bool {
    left.thread_id == right.thread_id
        && left.agent_key == right.agent_key
        && left.session_id == right.session_id
        && left.project_path == right.project_path
}

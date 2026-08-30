//! State shared by ACP notification callbacks and the Discord renderer.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::schema::v1::SessionConfigOption;
use tokio::sync::mpsc;

use crate::{
    acp::{model::SessionUiState, runtime::Signal},
    discord::render::ProjectionEvent,
};

/// Notification and lifecycle state for one active ACP connection.
#[derive(Clone, Debug)]
pub(super) struct ProjectionState {
    /// Ordered update queue consumed by the renderer task.
    pub(super) updates: mpsc::Sender<ProjectionEvent>,
    /// Current prompt identifier for unkeyed ACP chunks.
    pub(super) current_turn: Arc<Mutex<String>>,
    /// Whether the connection is replaying persisted session history.
    pub(super) replaying: Arc<Mutex<bool>>,
    /// Signals that the renderer cannot keep up with ACP.
    pub(super) fault: Arc<Signal>,
    /// Stops the actor and its ACP connection.
    pub(super) stop: Arc<Signal>,
    /// Cached session configuration advertised by the agent.
    pub(super) ui: Arc<Mutex<SessionUiState>>,
}

impl ProjectionState {
    /// Returns the current logical turn captured by notification callbacks.
    pub(super) fn turn(&self) -> String {
        self.current_turn
            .lock()
            .expect("acp turn mutex poisoned")
            .clone()
    }

    /// Returns whether notifications are still replaying session history.
    pub(super) fn is_replaying(&self) -> bool {
        *self.replaying.lock().expect("acp replay mutex poisoned")
    }

    /// Changes the logical turn used for unkeyed notifications.
    pub(super) fn set_turn(&self, turn: String) {
        *self.current_turn.lock().expect("acp turn mutex poisoned") = turn;
    }

    /// Marks replay completion after `session/load` responds.
    pub(super) fn finish_replay(&self) {
        *self.replaying.lock().expect("acp replay mutex poisoned") = false;
    }

    /// Replaces the cached session configuration options.
    pub(super) fn apply_config_options(&self, options: Vec<SessionConfigOption>) {
        self.ui
            .lock()
            .expect("acp session ui mutex poisoned")
            .apply_config_options(options);
    }

    /// Returns a snapshot of the agent-advertised configuration options.
    pub(super) fn ui(&self) -> SessionUiState {
        self.ui
            .lock()
            .expect("acp session ui mutex poisoned")
            .clone()
    }
}

/// Collects adjacent projection events during the configured debounce window.
pub(super) async fn collect_batch(
    first: ProjectionEvent,
    receiver: &mut mpsc::Receiver<ProjectionEvent>,
    debounce: Duration,
) -> Vec<ProjectionEvent> {
    let mut events = vec![first];
    if debounce.is_zero() {
        return events;
    }

    let timer = tokio::time::sleep(debounce);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            event = receiver.recv() => {
                let Some(event) = event else {
                    break;
                };
                events.push(event);
            }
            () = &mut timer => break,
        }
    }
    events
}

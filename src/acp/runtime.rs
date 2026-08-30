//! Runtime signals shared by ACP actors and request helpers.

use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
};

/// One-shot notification used to stop an actor or report a queue fault.
#[derive(Debug)]
pub(super) struct Signal {
    /// Atomic state checked before awaiting the notification.
    triggered: AtomicBool,
    /// Wakes the actor that is waiting for the signal.
    notify: tokio::sync::Notify,
}

impl Signal {
    /// Marks the signal and wakes one waiter.
    pub(super) fn trigger(&self) {
        self.triggered.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Reads whether the signal has already fired.
    pub(super) fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    /// Waits until the signal fires.
    pub(super) async fn notified(&self) {
        self.notify.notified().await;
    }
}

impl Default for Signal {
    /// Creates a signal that has not fired yet.
    fn default() -> Self {
        Self {
            triggered: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
}

/// Runs an operation unless the actor is stopped first.
pub(super) async fn stop_aware<T>(
    stop: &Signal,
    operation: impl Future<Output = Result<T, agent_client_protocol::Error>>,
) -> Result<T, agent_client_protocol::Error> {
    if stop.is_triggered() {
        return Err(
            agent_client_protocol::Error::internal_error().data("acp session actor was stopped")
        );
    }
    tokio::select! {
        result = operation => result,
        () = stop.notified() => Err(agent_client_protocol::Error::internal_error()
            .data("acp session actor was stopped")),
    }
}

//! Relaying user messages to herdr agents: one FIFO worker per agent pane,
//! delivering each prompt immediately (no waiting on the turn — the 1s poll
//! mirrors the response, and the event loop posts the blocked notice on the
//! blocked status event).

use std::{collections::HashMap, sync::Arc, time::Duration};

use serenity::all::{ChannelId, Context, CreateMessage};
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::{
    error::BotError,
    herdr::{Herdr, PaneId},
};

/// A user message queued for an agent.
#[derive(Debug)]
pub struct RelayJob {
    /// The forum post thread to echo output into.
    pub channel_id: ChannelId,
    /// The user's message text.
    pub text: String,
}

/// Relays user messages to herdr agents, one job at a time per agent. A
/// delivered prompt is never waited on: the poll mirrors the agent's
/// response (within a tick), and the event loop posts the blocked notice
/// when the agent's status turns blocked. This task's only job is getting
/// the words to the agent.
///
/// Clones share the worker map: workers spawn on a clone and must remove
/// their own entry from the same map `submit` consults.
#[derive(Debug, Clone)]
pub struct Relay {
    herdr: Herdr,
    /// How long a worker with no incoming messages stays alive.
    idle_timeout: Duration,
    /// Live workers by pane id — agents are unnamed, so the pane is the
    /// stable relay target.
    workers: Arc<Mutex<HashMap<PaneId, mpsc::Sender<RelayJob>>>>,
}

impl Relay {
    /// Creates a relay whose per-agent workers die after `idle_timeout` of
    /// silence.
    #[must_use]
    pub fn new(herdr: Herdr, idle_timeout: Duration) -> Self {
        Self {
            herdr,
            idle_timeout,
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Queues a job for an agent pane, spawning its worker if needed. Jobs
    /// for one pane are processed in order.
    pub async fn submit(
        &self,
        ctx: Context,
        target: &PaneId,
        job: RelayJob,
    ) -> Result<(), BotError> {
        // Grab the pane's sender without holding the lock across the
        // awaited send: one busy pane must not stall every pane's relay.
        let sender = self.workers.lock().await.get(target).cloned();

        if let Some(sender) = sender {
            return sender
                .send(job)
                .await
                .map_err(|_| BotError::Other(format!("relay worker for `{target}` stopped")));
        }

        let (sender, receiver) = mpsc::channel(32);
        let worker_sender = sender.clone();
        self.workers
            .lock()
            .await
            .insert(target.to_owned(), worker_sender.clone());

        let relay = self.clone();
        let worker_target = target.to_owned();
        tokio::spawn(async move {
            relay
                .run_worker(ctx, worker_target, worker_sender, receiver)
                .await;
        });

        sender
            .send(job)
            .await
            .map_err(|_| BotError::Other(format!("relay worker for `{target}` stopped")))
    }

    async fn run_worker(
        &self,
        ctx: Context,
        target: PaneId,
        sender: mpsc::Sender<RelayJob>,
        mut receiver: mpsc::Receiver<RelayJob>,
    ) {
        loop {
            let Ok(Some(job)) = tokio::time::timeout(self.idle_timeout, receiver.recv()).await
            else {
                break;
            };

            if let Err(error) = self.process_job(&ctx, &target, job).await {
                warn!(?error, %target, "relay job failed");
            }
        }

        // Only remove our own entry so a fresh worker that took over isn't clobbered.
        let mut workers = self.workers.lock().await;
        if let Some(current) = workers.get(&target)
            && current.same_channel(&sender)
        {
            workers.remove(&target);
        }
    }

    /// Delivers one prompt to the agent: herdr writes it to the agent's
    /// input and answers at once, so the worker moves straight on — a long
    /// turn never holds the queue, and later messages reach the agent as
    /// they arrive instead of sitting invisible behind it.
    async fn process_job(
        &self,
        ctx: &Context,
        target: &PaneId,
        job: RelayJob,
    ) -> Result<(), BotError> {
        if let Err(error) = self.herdr.send_prompt(target, &job.text).await {
            job.channel_id
                .widen()
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!("error talking to `{target}`: {error}")),
                )
                .await?;
            return Err(error.into());
        }
        Ok(())
    }
}

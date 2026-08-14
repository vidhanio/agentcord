use std::{
    collections::HashMap,
    fmt::{self, Display, Formatter},
    sync::Arc,
    time::Duration,
};

use serenity::all::{ChannelId, Context, CreateMessage};
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::{
    error::BotError,
    forum::Forum,
    herdr::{Agent, AgentStatus, Herdr, PaneId, SessionPath},
};

/// A user message queued for an agent.
#[derive(Debug)]
pub struct RelayJob {
    /// The forum post thread to echo output into.
    pub channel_id: ChannelId,
    /// The agent's session transcript path, synced once the prompt settles.
    pub session_path: SessionPath,
    /// The user's message text.
    pub text: String,
}

/// How long a worker with no incoming messages stays alive.
const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// What a failed herdr call was doing, for error messages.
#[derive(Debug, Clone, Copy)]
enum HerdrAction {
    /// Sending a prompt.
    Talk,
    /// Waiting for a settled state.
    Wait,
}

impl Display for HerdrAction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Talk => "talking to",
            Self::Wait => "waiting for",
        })
    }
}

/// Relays user messages to herdr agents, one job at a time per agent, and
/// syncs the agent's session transcript back to the forum post.
///
/// Clones share the worker map: workers spawn on a clone and must remove
/// their own entry from the same map `submit` consults.
#[derive(Debug, Clone)]
pub struct Relay {
    herdr: Herdr,
    forum: Arc<Forum>,
    /// Live workers by pane id — agents are unnamed, so the pane is the
    /// stable relay target.
    workers: Arc<Mutex<HashMap<PaneId, mpsc::Sender<RelayJob>>>>,
}

impl Relay {
    #[must_use]
    pub fn new(herdr: Herdr, forum: Arc<Forum>) -> Self {
        Self {
            herdr,
            forum,
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
            let Ok(Some(job)) = tokio::time::timeout(WORKER_IDLE_TIMEOUT, receiver.recv()).await
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

    async fn process_job(
        &self,
        ctx: &Context,
        target: &PaneId,
        job: RelayJob,
    ) -> Result<(), BotError> {
        // A typing indicator while the job runs; dropped (stopping it) when
        // the job settles.
        let _typing = serenity::all::Typing::start(Arc::clone(&ctx.http), job.channel_id);

        let prompt_timeout = crate::config::PROMPT_TIMEOUT;

        // `prompt_agent` waits for the turn to settle; a timeout error means
        // the agent is still working. Keep waiting silently — the typing
        // indicator stays on — and sync the transcript once the turn settles.
        let agent = match self
            .herdr
            .prompt_agent(target, &job.text, prompt_timeout)
            .await
        {
            Ok(agent) => agent,
            // The agent is still working (timeout) or accepted the prompt
            // without herdr observing a state change within its short
            // window (stall). Keep waiting silently — the typing indicator
            // stays on — and sync the transcript once the turn settles.
            Err(error) if error.is_timeout() || error.is_stalled() => {
                match self
                    .wait_until_settled(ctx, target, job.channel_id, prompt_timeout)
                    .await
                {
                    Ok(next) => next,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => {
                self.post_error(ctx, job.channel_id, target, HerdrAction::Talk, &error)
                    .await?;
                return Err(error.into());
            }
        };

        // The agent's output is synced from its session file; a sync failure
        // is best-effort (the periodic reconcile retries).
        self.forum
            .sync_session_by_path(ctx, &job.session_path)
            .await;

        if agent.status() == AgentStatus::Blocked {
            job.channel_id
                .send_message(
                    ctx,
                    CreateMessage::new()
                        .content("The agent is **blocked** — it's waiting for input."),
                )
                .await?;
        }

        Ok(())
    }

    /// Posts an error message about a failed herdr call.
    async fn post_error(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
        target: &PaneId,
        action: HerdrAction,
        error: &crate::herdr::Error,
    ) -> Result<(), BotError> {
        channel_id
            .send_message(
                ctx,
                CreateMessage::new().content(format!("Error {action} `{target}`: {error}")),
            )
            .await?;
        Ok(())
    }

    /// Waits for the agent to leave the working state.
    async fn wait_until_settled(
        &self,
        ctx: &Context,
        target: &PaneId,
        channel_id: ChannelId,
        timeout: Duration,
    ) -> Result<Agent, BotError> {
        loop {
            match self.herdr.wait_agent(target, timeout).await {
                Ok(next) => return Ok(next),
                // Still working: the wait timed out, keep going.
                Err(error) if error.is_timeout() => {}
                Err(error) => {
                    self.post_error(ctx, channel_id, target, HerdrAction::Wait, &error)
                        .await?;
                    return Err(error.into());
                }
            }
        }
    }
}

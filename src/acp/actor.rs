//! ACP actor lifecycle and renderer task wiring.

use std::sync::{Arc, Mutex};

use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use tokio::sync::mpsc;
use tracing::warn;

use super::{
    connection,
    projection::{ProjectionState, collect_batch},
    registry::{ActorStartup, SessionCommand},
    runtime::Signal,
};
use crate::{Bot, db::SessionRow};

/// Maximum number of ACP updates waiting for the renderer.
const UPDATE_QUEUE_CAPACITY: usize = 128;

/// Runs the renderer task and one ACP connection for a persisted session.
pub(super) async fn run(
    bot: Bot,
    row: SessionRow,
    commands: mpsc::Receiver<SessionCommand>,
    stop: Arc<Signal>,
    ui: Arc<Mutex<super::model::SessionUiState>>,
    startup: ActorStartup,
) -> Result<(), agent_client_protocol::Error> {
    let (updates, mut update_receiver) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
    let projection = ProjectionState {
        updates,
        current_turn: Arc::new(Mutex::new(String::from("restore"))),
        replaying: Arc::new(Mutex::new(true)),
        fault: Arc::new(Signal::default()),
        stop,
        ui,
    };
    let thread = row.thread_id;
    let edit_debounce = bot.config().timeouts.edit_debounce;
    let render_bot = bot.clone();
    let render_task = tokio::spawn(async move {
        while let Some(first) = update_receiver.recv().await {
            let events = collect_batch(first, &mut update_receiver, edit_debounce).await;
            if let Err(error) = render_bot.apply_projection_events(events).await {
                warn!(?error, thread = ?thread, "failed to project acp update");
            }
        }
    });

    let projection_updates = projection.updates.clone();
    let result = match startup {
        ActorStartup::New(session) => {
            connection::run_new(bot, row, commands, projection, session).await
        }
        ActorStartup::Restore => {
            if let Some(agent) = bot.config().agents.get(&row.agent_key).cloned() {
                let process = AcpAgent::new(
                    AcpAgentConfig::new(agent.command)
                        .args(agent.args)
                        .envs(agent.env),
                );
                connection::connect(bot, row, process, commands, projection).await
            } else {
                Err(agent_client_protocol::Error::invalid_request()
                    .data(format!("agent `{}` is no longer configured", row.agent_key)))
            }
        }
    };

    drop(projection_updates);
    if let Err(error) = render_task.await {
        warn!(?error, thread = ?thread, "projection task stopped unexpectedly");
    }
    result
}

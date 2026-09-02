//! ACP actor lifecycle and renderer task wiring.

use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionConfigOption;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{
    ContextUsage, connection,
    projection::{ProjectionState, collect_batch},
    protocol,
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
    config_options: Arc<Mutex<Vec<SessionConfigOption>>>,
    context_usage: Arc<Mutex<Option<ContextUsage>>>,
    startup: ActorStartup,
) -> Result<(), agent_client_protocol::Error> {
    info!(
        agent = %row.agent_key,
        session = %row.session_id,
        thread = ?row.thread_id,
        "starting acp actor..."
    );
    let (updates, mut update_receiver) = mpsc::channel(UPDATE_QUEUE_CAPACITY);
    let projection = ProjectionState {
        updates,
        current_turn: Arc::new(Mutex::new(String::from("restore"))),
        replaying: Arc::new(Mutex::new(true)),
        fault: Arc::new(Signal::default()),
        stop,
        config_options,
        context_usage,
    };
    let thread = row.thread_id;
    let edit_debounce = bot.config().timeouts.edit_debounce;
    let render_bot = bot.clone();
    let render_fault = Arc::clone(&projection.fault);
    let render_task = tokio::spawn(async move {
        while let Some(first) = update_receiver.recv().await {
            let events = collect_batch(first, &mut update_receiver, edit_debounce).await;
            if let Err(error) = render_bot.apply_projection_events(events).await {
                render_fault.trigger();
                warn!(?error, thread = ?thread, "failed to project acp update");
                break;
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
                let process = protocol::process(agent);
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
    match &result {
        Ok(()) => info!(thread = ?thread, "acp actor finished"),
        Err(error) => warn!(?error, thread = ?thread, "acp actor failed"),
    }
    result
}

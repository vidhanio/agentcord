//! Prompt execution and actor command handling.

use std::time::Duration;

use agent_client_protocol::{
    Agent, ConnectionTo,
    schema::v1::{ContentBlock, PromptRequest, SessionId},
};
use serenity::{
    all::{CreateMessage, GenericChannelId},
    model::mention::Mentionable,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use super::{
    model::{ModelSpec, apply_model},
    projection::ProjectionState,
    registry::SessionCommand,
    runtime::Signal,
};
use crate::{Bot, BotError};

/// Grace period after asking ACP to cancel a timed-out prompt.
const PROMPT_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Processes queued prompts serially on the restored ACP connection.
pub(super) async fn run_commands(
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
                .data("acp projection queue overflowed"));
        }
        if projection.stop.is_triggered() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("acp session actor was stopped"));
        }
        let command = tokio::select! {
            () = projection.fault.notified() => {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("acp projection queue overflowed"));
            }
            () = projection.stop.notified() => {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("acp session actor was stopped"));
            }
            () = connection.incoming_closed() => {
                return Err(agent_client_protocol::Error::internal_error()
                    .data("acp connection closed"));
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            break;
        };
        match command {
            SessionCommand::Prompt { text, turn_id } => {
                handle_prompt_command(
                    PromptContext {
                        bot: &bot,
                        connection: &connection,
                        session_id: &session_id,
                        thread,
                        projection,
                    },
                    text,
                    turn_id,
                )
                .await?;
            }
            SessionCommand::SetModel { model, done } => {
                handle_model_command(
                    &bot,
                    &connection,
                    &session_id,
                    thread,
                    projection,
                    model,
                    done,
                )
                .await;
            }
        }
        if projection.fault.is_triggered() {
            return Err(agent_client_protocol::Error::internal_error()
                .data("acp projection queue overflowed"));
        }
    }
    if projection.fault.is_triggered() {
        return Err(
            agent_client_protocol::Error::internal_error().data("acp projection queue overflowed")
        );
    }
    Ok(())
}

/// Sends one queued prompt through the active ACP session.
struct PromptContext<'a> {
    bot: &'a Bot,
    connection: &'a ConnectionTo<Agent>,
    session_id: &'a SessionId,
    thread: GenericChannelId,
    projection: &'a ProjectionState,
}

/// Handles one queued prompt using the active session context.
async fn handle_prompt_command(
    context: PromptContext<'_>,
    text: String,
    turn_id: String,
) -> Result<(), agent_client_protocol::Error> {
    let PromptContext {
        bot,
        connection,
        session_id,
        thread,
        projection,
    } = context;
    projection.set_turn(turn_id);

    match run_prompt(
        bot,
        connection,
        session_id.clone(),
        thread,
        text,
        &projection.stop,
        &projection.fault,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(PromptFailure::Prompt(error)) => {
            let error = BotError::AcpProtocol(error.to_string());
            notify_failure(bot, thread, format!("acp `session/prompt` failed: {error}")).await;
            warn!(?error, thread = ?thread, "acp `session/prompt` failed");
            Ok(())
        }
        Err(PromptFailure::Connection(error)) => Err(error),
    }
}

/// Applies one queued model selection and reports its result to the caller.
async fn handle_model_command(
    bot: &Bot,
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    thread: GenericChannelId,
    projection: &ProjectionState,
    model: ModelSpec,
    done: oneshot::Sender<
        Result<
            Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
            agent_client_protocol::Error,
        >,
    >,
) {
    let requested_model = model.clone();
    let config_options = projection.config_options();
    let result = apply_model(
        connection,
        session_id,
        &config_options,
        &model,
        bot.config().timeouts.startup,
        Some(&projection.stop),
    )
    .await;
    if let Ok(options) = &result {
        projection.set_config_options(options.clone());
    } else if let Err(error) = &result {
        warn!(
            ?error,
            thread = ?thread,
            session = %session_id,
            model = %requested_model,
            "failed to apply acp model selection"
        );
    }
    let _ = done.send(result);
}

/// Sends one prompt and enforces timeout/cancellation behavior.
async fn run_prompt(
    bot: &Bot,
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    thread: GenericChannelId,
    text: String,
    stop: &Signal,
    fault: &Signal,
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
                                .data(format!("acp `session/prompt` task failed: {error}")),
                        )
                    })
                    .and_then(|result| result.map_err(PromptFailure::Prompt));
            }
        }
        () = stop.notified() => {
            info!(thread = ?thread, session = %session_id, "cancelling stopped acp `session/prompt`...");
            if let Err(error) = connection.send_notification(
                agent_client_protocol::schema::v1::CancelNotification::new(session_id.clone()),
            ) {
                warn!(?error, "failed to send acp `session/cancel` for stopped prompt");
            }
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                .data("acp session actor was stopped"),
            ));
        }
        () = fault.notified() => {
            if let Err(error) = connection.send_notification(
                agent_client_protocol::schema::v1::CancelNotification::new(session_id.clone()),
            ) {
                warn!(?error, "failed to cancel prompt after projection failure");
            }
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                    .data("acp projection failed during prompt"),
            ));
        }
    }

    info!(thread = ?thread, session = %session_id, "cancelling timed-out acp `session/prompt`...");
    if let Err(error) = connection.send_notification(
        agent_client_protocol::schema::v1::CancelNotification::new(session_id.clone()),
    ) {
        warn!(?error, thread = ?thread, session = %session_id, "failed to send acp `session/cancel` after prompt timeout");
        request_task.abort();
        return Err(PromptFailure::Connection(
            agent_client_protocol::Error::internal_error()
                .data(format!("failed to send acp `session/cancel`: {error}")),
        ));
    }
    tokio::select! {
        result = tokio::time::timeout(PROMPT_CANCEL_GRACE, &mut request_task) => {
            if let Ok(result) = result {
                return result
                    .map_err(|error| {
                        PromptFailure::Connection(
                            agent_client_protocol::Error::internal_error()
                                .data(format!("acp cancelled `session/prompt` task failed: {error}")),
                        )
                    })
                    .and_then(|result| result.map_err(PromptFailure::Prompt));
            }
        }
        () = stop.notified() => {
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                .data("acp session actor was stopped"),
            ));
        }
        () = fault.notified() => {
            request_task.abort();
            return Err(PromptFailure::Connection(
                agent_client_protocol::Error::internal_error()
                    .data("acp projection failed during prompt cancellation"),
            ));
        }
    }
    request_task.abort();
    Err(PromptFailure::Connection(
        agent_client_protocol::Error::internal_error().data(format!(
            "acp `session/prompt` did not finish after `session/cancel` (thread {})",
            thread.mention()
        )),
    ))
}

/// Distinguishes a prompt rejection from a broken ACP connection.
#[derive(Debug)]
enum PromptFailure {
    /// The agent rejected only this prompt.
    Prompt(agent_client_protocol::Error),
    /// The connection can no longer process this session.
    Connection(agent_client_protocol::Error),
}

/// Reports an actor failure in its Discord thread when possible.
pub(super) async fn notify_failure(bot: &Bot, thread: GenericChannelId, message: String) {
    let Ok(context) = bot.context() else {
        warn!(
            ?thread,
            "failed to report acp actor failure: discord is not ready"
        );
        return;
    };
    if let Err(error) = thread
        .send_message(&context.http, CreateMessage::new().content(message))
        .await
    {
        warn!(?error, ?thread, "failed to report acp actor failure");
    }
}

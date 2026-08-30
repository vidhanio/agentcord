//! ACP connection callbacks and session restoration.

use agent_client_protocol::{
    AcpAgent, Agent, Client, ConnectionTo, Dispatch, HandleDispatchFrom, Handled, Responder,
    schema::v1::{
        LoadSessionRequest, RequestPermissionRequest, RequestPermissionResponse, SessionId,
        SessionNotification,
    },
    util::MatchDispatchFrom,
};
use serenity::all::GenericChannelId;
use tokio::sync::mpsc;
use tracing::warn;

use super::{
    projection::ProjectionState,
    prompt::run_commands,
    protocol::{NewSessionConnection, initialize, request_with_timeout},
    registry::SessionCommand,
    runtime::stop_aware,
};
use crate::{
    Bot,
    db::SessionRow,
    discord::{permission, render::ProjectionEvent},
};

/// Initializes a new ACP connection and restores one persisted session.
pub(super) async fn connect(
    bot: Bot,
    row: SessionRow,
    process: AcpAgent,
    mut commands: mpsc::Receiver<SessionCommand>,
    projection: ProjectionState,
) -> Result<(), agent_client_protocol::Error> {
    Client
        .builder()
        .name("agentcord")
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            run_connected(bot, row, connection, projection, &mut commands, true).await
        })
        .await
}

/// Runs a newly created session on the live connection returned by
/// `session/new`, without attempting the rollout-backed `session/load` path.
pub(super) async fn run_new(
    bot: Bot,
    row: SessionRow,
    mut commands: mpsc::Receiver<SessionCommand>,
    projection: ProjectionState,
    startup: NewSessionConnection,
) -> Result<(), agent_client_protocol::Error> {
    let NewSessionConnection {
        connection,
        task,
        release,
    } = startup;
    let result = run_connected(bot, row, connection, projection, &mut commands, false).await;
    let _ = release.send(());
    let task_result = task.await.map_err(|error| {
        agent_client_protocol::Error::internal_error()
            .data(format!("acp new session task failed: {error}"))
    })?;
    match result {
        Ok(()) => task_result,
        Err(error) => Err(error),
    }
}

/// Installs the session handler and runs either restoration or live commands.
async fn run_connected(
    bot: Bot,
    row: SessionRow,
    connection: ConnectionTo<Agent>,
    projection: ProjectionState,
    commands: &mut mpsc::Receiver<SessionCommand>,
    restore: bool,
) -> Result<(), agent_client_protocol::Error> {
    let _handler = connection.add_dynamic_handler(SessionHandler::new(
        bot.clone(),
        row.clone(),
        projection.clone(),
    ))?;
    if restore {
        restore_session(bot, row, connection, projection, commands).await
    } else {
        projection.finish_replay();
        run_commands(
            bot,
            connection,
            row.session_id,
            row.thread_id,
            &projection,
            commands,
        )
        .await
    }
}

/// Routes session updates and permission requests for one active actor.
struct SessionHandler {
    /// Shared application state used to resolve Discord permissions.
    bot: Bot,
    /// Agent-owned session identifier accepted by this handler.
    expected_session: SessionId,
    /// Discord thread receiving projected updates.
    thread: GenericChannelId,
    /// Renderer and in-memory config state for this session.
    projection: ProjectionState,
}

impl SessionHandler {
    /// Creates a handler bound to one Agentcord session.
    fn new(bot: Bot, row: SessionRow, projection: ProjectionState) -> Self {
        Self {
            bot,
            expected_session: row.session_id,
            thread: row.thread_id,
            projection,
        }
    }
}

impl HandleDispatchFrom<Agent> for SessionHandler {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        connection: ConnectionTo<Agent>,
    ) -> Result<Handled<Dispatch>, agent_client_protocol::Error> {
        let notification_projection = self.projection.clone();
        let notification_session = self.expected_session.clone();
        let notification_thread = self.thread;
        let permission_bot = self.bot.clone();
        let permission_session = self.expected_session.clone();
        let permission_thread = self.thread;
        let permission_connection = connection.clone();

        MatchDispatchFrom::new(message, &connection)
            .if_notification(move |notification: SessionNotification| {
                let projection = notification_projection.clone();
                let expected_session = notification_session.clone();
                async move {
                    enqueue_notification(
                        &projection,
                        &expected_session,
                        notification_thread,
                        notification,
                    )
                }
            })
            .await
            .if_request(move |request: RequestPermissionRequest, responder| {
                let bot = permission_bot.clone();
                let expected_session = permission_session.clone();
                let connection = permission_connection.clone();
                async move {
                    if request.session_id != expected_session {
                        warn!(
                            expected = %expected_session,
                            received = %request.session_id,
                            thread = ?permission_thread,
                            "ignoring acp permission request for another session"
                        );
                        return respond_permission(
                            responder,
                            permission::cancelled(),
                            permission_thread,
                        );
                    }
                    if bot.config().permissions.approve_all {
                        return respond_permission(
                            responder,
                            permission::approve_all(&request),
                            permission_thread,
                        );
                    }
                    let Some(context) = discord_context(&bot, permission_thread) else {
                        warn!(
                            thread = ?permission_thread,
                            "cancelling permission request without discord context"
                        );
                        return respond_permission(
                            responder,
                            permission::cancelled(),
                            permission_thread,
                        );
                    };
                    let permission_user = bot.config().discord.allowed_user_id;
                    let permission_timeout = bot.config().timeouts.permission;
                    connection
                        .spawn(async move {
                            let response = permission::ask(
                                context,
                                permission_thread,
                                permission_user,
                                permission_timeout,
                                request,
                            )
                            .await;
                            respond_permission(responder, response, permission_thread)
                        })
                        .map_err(|error| {
                            warn!(
                                ?error,
                                thread = ?permission_thread,
                                "failed to schedule permission request"
                            );
                            error
                        })
                }
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "agentcord session handler"
    }
}

/// Enqueues one session notification for the renderer.
fn enqueue_notification(
    projection: &ProjectionState,
    expected_session: &SessionId,
    thread: GenericChannelId,
    notification: SessionNotification,
) -> Result<(), agent_client_protocol::Error> {
    if notification.session_id != *expected_session {
        warn!(
            expected = %expected_session,
            received = %notification.session_id,
            thread = ?thread,
            "ignoring acp notification for another session"
        );
        return Ok(());
    }
    if let SessionNotification {
        update: agent_client_protocol::schema::v1::SessionUpdate::ConfigOptionUpdate(update),
        ..
    } = &notification
    {
        projection.apply_config_options(update.config_options.clone());
    }
    projection
        .updates
        .try_send(ProjectionEvent {
            thread_id: thread,
            turn_id: projection.turn(),
            replay: projection.is_replaying(),
            update: notification.update,
        })
        .map_err(|error| {
            projection.fault.trigger();
            let message = match error {
                mpsc::error::TrySendError::Full(_) => "the acp projection queue is full",
                mpsc::error::TrySendError::Closed(_) => {
                    "the acp projection task is no longer running"
                }
            };
            agent_client_protocol::Error::internal_error().data(message)
        })
}

/// Clones the Discord context used by permission interaction tasks.
fn discord_context(bot: &Bot, thread: GenericChannelId) -> Option<serenity::all::Context> {
    match bot.context() {
        Ok(context) => Some(context.clone()),
        Err(error) => {
            warn!(
                ?error,
                ?thread,
                "permission handler started without discord context"
            );
            None
        }
    }
}

/// Initializes and restores an ACP connection before processing commands.
async fn restore_session(
    bot: Bot,
    row: SessionRow,
    connection: ConnectionTo<Agent>,
    projection: ProjectionState,
    commands: &mut mpsc::Receiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    let initialized = stop_aware(
        &projection.stop,
        initialize(&connection, bot.config().timeouts.startup),
    )
    .await?;
    if !initialized.agent_capabilities.load_session {
        return Err(agent_client_protocol::Error::invalid_request()
            .data("agent does not advertise session/load"));
    }
    let loaded = stop_aware(
        &projection.stop,
        request_with_timeout(
            bot.config().timeouts.startup,
            connection
                .send_request(LoadSessionRequest::new(
                    row.session_id.clone(),
                    row.project_path.clone(),
                ))
                .block_task(),
            "acp session/load timed out",
        ),
    )
    .await?;
    if let Some(options) = loaded.config_options {
        projection.apply_config_options(options);
    }
    if projection.fault.is_triggered() {
        return Err(agent_client_protocol::Error::internal_error()
            .data("acp projection queue overflowed during session/load"));
    }
    projection.finish_replay();
    run_commands(
        bot,
        connection,
        row.session_id,
        row.thread_id,
        &projection,
        commands,
    )
    .await
}

/// Sends a permission response and logs transport failures.
fn respond_permission(
    responder: Responder<RequestPermissionResponse>,
    response: RequestPermissionResponse,
    thread: GenericChannelId,
) -> Result<(), agent_client_protocol::Error> {
    responder.respond(response).map_err(|error| {
        warn!(?error, thread = ?thread, "failed to send acp permission response");
        error
    })
}

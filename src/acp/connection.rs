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
use tracing::{info, warn};

use super::{
    model::default_model,
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
    let result = Client
        .builder()
        .name("agentcord")
        .connect_with(process, |connection: ConnectionTo<Agent>| async move {
            run_connected(bot, row, connection, projection, &mut commands, true).await
        })
        .await;
    if let Err(error) = &result {
        warn!(?error, "acp `session/load` connection failed");
    }
    result
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
    info!(
        agent = %row.agent_key,
        session = %row.session_id,
        thread = ?row.thread_id,
        "starting live acp session..."
    );
    let result = run_connected(bot, row, connection, projection, &mut commands, false).await;
    let _ = release.send(());
    let task_result = task.await.map_err(|error| {
        agent_client_protocol::Error::internal_error()
            .data(format!("acp `session/new` task failed: {error}"))
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
    let _handler = match connection.add_dynamic_handler(SessionHandler::new(
        bot.clone(),
        row.clone(),
        projection.clone(),
    )) {
        Ok(handler) => handler,
        Err(error) => {
            warn!(
                ?error,
                session = %row.session_id,
                thread = ?row.thread_id,
                "failed to install acp session handler"
            );
            return Err(error);
        }
    };
    if restore {
        info!(
            agent = %row.agent_key,
            session = %row.session_id,
            "restoring acp session..."
        );
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
    /// Persisted session metadata used for Discord session presentation.
    row: SessionRow,
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
            expected_session: row.session_id.clone(),
            thread: row.thread_id,
            row,
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
        let notification_bot = self.bot.clone();
        let notification_row = self.row.clone();
        let permission_bot = self.bot.clone();
        let permission_session = self.expected_session.clone();
        let permission_thread = self.thread;
        let permission_connection = connection.clone();

        MatchDispatchFrom::new(message, &connection)
            .if_notification(move |notification: SessionNotification| {
                let bot = notification_bot.clone();
                let projection = notification_projection.clone();
                let expected_session = notification_session.clone();
                let row = notification_row.clone();
                async move {
                    enqueue_notification(
                        &bot,
                        &row,
                        &projection,
                        &expected_session,
                        notification_thread,
                        notification,
                    )
                    .await
                }
            })
            .await
            .if_request(move |request: RequestPermissionRequest, responder| {
                let bot = permission_bot.clone();
                let expected_session = permission_session.clone();
                let connection = permission_connection.clone();
                async move {
                    handle_permission_request(
                        &bot,
                        &expected_session,
                        permission_thread,
                        &connection,
                        request,
                        responder,
                    )
                }
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "agentcord session handler"
    }
}

/// Routes one ACP permission request to automatic or Discord-backed handling.
fn handle_permission_request(
    bot: &Bot,
    expected_session: &SessionId,
    thread: GenericChannelId,
    connection: &ConnectionTo<Agent>,
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
) -> Result<(), agent_client_protocol::Error> {
    if request.session_id != *expected_session {
        warn!(
            expected = %expected_session,
            received = %request.session_id,
            thread = ?thread,
            "ignoring acp permission request for another session"
        );
        return respond_permission(responder, permission::cancelled(), thread);
    }
    if bot.config().permissions.approve_all {
        return respond_permission(responder, permission::approve_all(&request), thread);
    }
    let Some(context) = discord_context(bot, thread) else {
        warn!(
            thread = ?thread,
            "cancelling permission request without discord context..."
        );
        return respond_permission(responder, permission::cancelled(), thread);
    };
    let permission_user = bot.config().discord.allowed_user_id;
    let permission_timeout = bot.config().timeouts.permission;
    connection
        .spawn(async move {
            let response = permission::ask(
                context,
                thread,
                permission_user,
                permission_timeout,
                request,
            )
            .await;
            respond_permission(responder, response, thread)
        })
        .map_err(|error| {
            warn!(?error, thread = ?thread, "failed to schedule permission request");
            error
        })
}

/// Enqueues one session notification for the renderer.
async fn enqueue_notification(
    bot: &Bot,
    row: &SessionRow,
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
        projection.set_config_options(update.config_options.clone());
        let current_model = default_model(&update.config_options);
        if let Err(error) = bot
            .update_session_starter(row, current_model.as_deref())
            .await
        {
            warn!(
                ?error,
                thread = ?thread,
                "failed to update session starter after acp `session/update`"
            );
        }
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
            warn!(
                ?error,
                session = %expected_session,
                thread = ?thread,
                "failed to queue acp session notification"
            );
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
        warn!(
            agent = %row.agent_key,
            session = %row.session_id,
            "acp agent does not advertise `session/load`"
        );
        return Err(agent_client_protocol::Error::invalid_request()
            .data("agent does not advertise `session/load`"));
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
            "acp `session/load` timed out",
        ),
    )
    .await?;
    if let Some(options) = loaded.config_options {
        projection.set_config_options(options);
    }
    let config_options = projection.config_options();
    let current_model = default_model(&config_options);
    if let Err(error) = bot
        .update_session_starter(&row, current_model.as_deref())
        .await
    {
        warn!(
            ?error,
            thread = ?row.thread_id,
            "failed to update session starter after acp `session/load`"
        );
    }
    if projection.fault.is_triggered() {
        return Err(agent_client_protocol::Error::internal_error()
            .data("acp projection queue overflowed during `session/load`"));
    }
    projection.finish_replay();
    info!(
        agent = %row.agent_key,
        session = %row.session_id,
        "acp session restored"
    );
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

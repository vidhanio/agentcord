//! ACP lifecycle and listing requests.

use std::{path::PathBuf, time::Duration};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo,
    schema::{
        ProtocolVersion,
        v1::{
            InitializeRequest, InitializeResponse, ListSessionsRequest, NewSessionRequest,
            SessionConfigOption, SessionId, SessionInfo,
        },
    },
};
use tokio::{sync::oneshot, task::JoinHandle};

use super::{Supervisor, acp_error, configured_agent};
use crate::{Bot, BotError, BotResult, config::AgentKey};

/// ACP metadata returned while creating a new session.
pub struct NewSession {
    /// Agent-owned session identifier.
    pub session_id: SessionId,
    /// Configuration options advertised by the agent for the new session.
    pub config_options: Vec<SessionConfigOption>,
    /// Live connection created by `session/new`.
    pub(super) connection: NewSessionConnection,
}

/// Live ACP connection retained until a newly created session actor starts.
pub(super) struct NewSessionConnection {
    /// Connection context shared with the session actor.
    pub(super) connection: ConnectionTo<Agent>,
    /// Future driving the ACP transport and startup foreground.
    pub(super) task: JoinHandle<Result<(), agent_client_protocol::Error>>,
    /// Releases the startup foreground once the actor has finished.
    pub(super) release: oneshot::Sender<()>,
}

/// ACP metadata returned while inspecting an existing session.
#[derive(Clone, Debug)]
pub struct ImportedSession {
    /// Agent-owned session identifier.
    pub session_id: SessionId,
    /// Working directory reported by the agent.
    pub project_path: PathBuf,
    /// Optional title reported by the agent.
    pub title: Option<String>,
}

/// Session information exposed by an agent's `session/list` implementation.
#[derive(Clone, Debug)]
pub struct ListedSession {
    /// Agent-owned session identifier.
    pub session_id: SessionId,
    /// Working directory reported by the agent.
    pub project_path: PathBuf,
    /// Optional title reported by the agent.
    pub title: Option<String>,
}

impl Supervisor {
    /// Creates a new ACP session and retains its connection for the actor.
    pub async fn new_session(
        &self,
        bot: &Bot,
        agent_key: &AgentKey,
        project_path: PathBuf,
    ) -> BotResult<NewSession> {
        let agent = configured_agent(bot, agent_key)?;
        let timeout = bot.config().timeouts.startup;
        let process = AcpAgent::new(
            AcpAgentConfig::new(agent.command)
                .args(agent.args)
                .envs(agent.env),
        );
        let (ready_sender, ready_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            Client
                .builder()
                .name("agentcord")
                .connect_with(process, |connection: ConnectionTo<Agent>| async move {
                    initialize(&connection, timeout).await?;
                    let response = request_with_timeout(
                        timeout,
                        connection
                            .send_request(NewSessionRequest::new(project_path))
                            .block_task(),
                        "acp session/new timed out",
                    )
                    .await?;
                    ready_sender
                        .send(NewSessionReady {
                            session_id: response.session_id,
                            config_options: response.config_options.unwrap_or_default(),
                            connection,
                        })
                        .map_err(|_| {
                            agent_client_protocol::Error::internal_error()
                                .data("acp new session startup receiver was dropped")
                        })?;
                    let _ = release_receiver.await;
                    Ok(())
                })
                .await
        });
        let Ok(ready) = ready_receiver.await else {
            let result = task.await.map_err(|error| {
                BotError::AcpProtocol(format!("acp new session task failed: {error}"))
            })?;
            return Err(match result {
                Ok(()) => {
                    BotError::AcpProtocol("acp new session ended before returning a session".into())
                }
                Err(error) => acp_error(&error),
            });
        };
        Ok(NewSession {
            session_id: ready.session_id,
            config_options: ready.config_options,
            connection: NewSessionConnection {
                connection: ready.connection,
                task,
                release: release_sender,
            },
        })
    }

    /// Lists all sessions exposed by an ACP agent, following pagination.
    pub async fn list_sessions(
        &self,
        bot: &Bot,
        agent_key: &AgentKey,
    ) -> BotResult<Vec<ListedSession>> {
        let agent = configured_agent(bot, agent_key)?;
        let timeout = bot.config().timeouts.startup;
        let process = AcpAgent::new(
            AcpAgentConfig::new(agent.command)
                .args(agent.args)
                .envs(agent.env),
        );
        Client
            .builder()
            .name("agentcord")
            .connect_with(process, |connection: ConnectionTo<Agent>| async move {
                let initialized = initialize(&connection, timeout).await?;
                if initialized
                    .agent_capabilities
                    .session_capabilities
                    .list
                    .is_none()
                {
                    return Err(agent_client_protocol::Error::invalid_request()
                        .data("agent does not advertise session/list"));
                }
                ListedSession::fetch_all(&connection, timeout).await
            })
            .await
            .map_err(|error| acp_error(&error))
    }

    /// Looks up one external session before it is imported into Discord.
    pub async fn inspect_session(
        &self,
        bot: &Bot,
        agent_key: &AgentKey,
        session_id: &SessionId,
    ) -> BotResult<ImportedSession> {
        self.list_sessions(bot, agent_key)
            .await?
            .into_iter()
            .find(|session| session.session_id == *session_id)
            .map(|session| ImportedSession {
                session_id: session.session_id,
                project_path: session.project_path,
                title: session.title,
            })
            .ok_or_else(|| BotError::SessionNotFound {
                session_id: session_id.to_string(),
            })
    }
}

/// Startup data transferred out of the `session/new` connection.
struct NewSessionReady {
    /// Agent-owned session identifier.
    session_id: SessionId,
    /// Configuration options advertised by the agent.
    config_options: Vec<SessionConfigOption>,
    /// Live connection context retained by the startup task.
    connection: ConnectionTo<Agent>,
}

/// Negotiates ACP v1 and validates the response.
pub(super) async fn initialize(
    connection: &ConnectionTo<Agent>,
    timeout: Duration,
) -> Result<InitializeResponse, agent_client_protocol::Error> {
    let response = tokio::time::timeout(
        timeout,
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task(),
    )
    .await
    .map_err(|_| {
        agent_client_protocol::Error::internal_error().data("acp initialize timed out")
    })??;
    if response.protocol_version != ProtocolVersion::V1 {
        return Err(agent_client_protocol::Error::invalid_request()
            .data("agent negotiated an unsupported acp protocol version"));
    }
    Ok(response)
}

/// Bounds one ACP request so an unresponsive connection cannot linger.
pub(super) async fn request_with_timeout<T>(
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<T, agent_client_protocol::Error>>,
    message: &'static str,
) -> Result<T, agent_client_protocol::Error> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error().data(message))?
}

impl ListedSession {
    /// Follows ACP session/list pagination and converts each result.
    pub(super) async fn fetch_all(
        connection: &ConnectionTo<Agent>,
        timeout: Duration,
    ) -> Result<Vec<Self>, agent_client_protocol::Error> {
        let mut sessions = Vec::new();
        let mut cursor = None;
        loop {
            let response = request_with_timeout(
                timeout,
                connection
                    .send_request(ListSessionsRequest::new().cursor(cursor.take()))
                    .block_task(),
                "acp session/list timed out",
            )
            .await?;
            sessions.extend(response.sessions.into_iter().map(Self::from_info));
            let Some(next_cursor) = response.next_cursor else {
                return Ok(sessions);
            };
            cursor = Some(next_cursor);
        }
    }

    /// Converts one ACP session-list record into the import representation.
    fn from_info(session: SessionInfo) -> Self {
        Self {
            session_id: session.session_id,
            project_path: session.cwd,
            title: session.title,
        }
    }
}

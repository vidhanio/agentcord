//! ACP subprocess supervision and session lifecycle.
//!
//! The public supervisor API is intentionally small. Each active session is
//! represented by one actor, while connection callbacks only enqueue ordered
//! updates for the renderer.

mod actor;
mod connection;
mod model;
mod projection;
mod prompt;
mod protocol;
mod registry;
mod runtime;

pub use model::{ModelSpec, SessionUiState, category_values, default_model};
pub use protocol::ListedSession;
pub use registry::Supervisor;

use crate::{
    Bot, BotError, BotResult,
    config::{AgentConfig, AgentKey},
};

/// Resolves one configured ACP executable by key.
pub fn configured_agent(bot: &Bot, agent_key: &AgentKey) -> BotResult<AgentConfig> {
    bot.config()
        .agents
        .get(agent_key)
        .cloned()
        .ok_or_else(|| BotError::UnknownAgent {
            key: agent_key.to_string(),
        })
}

/// Converts a protocol error into Agentcord's public error type.
pub fn acp_error(error: &agent_client_protocol::Error) -> BotError {
    BotError::AcpProtocol(error.to_string())
}

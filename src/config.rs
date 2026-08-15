//! Minimal configuration: only what is deployment-specific is read from the
//! environment. Everything else is a sane-default const here; knobs can be
//! promoted to env vars later if needed.

use std::{
    fmt::{self, Debug, Formatter},
    path::PathBuf,
    time::Duration,
};

use serde::Deserialize;
use serenity::all::{GuildId, UserId};

use crate::session::Harness;

/// Default harness for agents launched from forum posts without a harness
/// tag.
pub const DEFAULT_HARNESS: Harness = Harness::Pi;

/// How long one settle-wait call waits in a single request before the
/// relay continues waiting silently.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Timeout for individual herdr API calls.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the drift-backstop reconcile task runs; the event stream
/// handles updates instantly in between.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(600);

/// How often the transcript watcher refreshes its file watches (sessions
/// come and go) and checks for transcript rotations. New transcript
/// messages are mirrored on notify file events, not on this tick.
pub const MESSAGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How many unsynced transcript messages one sync posts at most when a
/// backlog beyond [`CATCHUP_BACKLOG`] is dropped, announced in small
/// italic text so it can never flood the thread.
pub const MAX_SYNC_MESSAGES: usize = 5;

/// A sync counts as a catch-up (and truncates to the last
/// [`MAX_SYNC_MESSAGES`]) only when the unsynced backlog exceeds this.
/// Normal turns — even heavy tool-call turns — are mirrored whole.
pub const CATCHUP_BACKLOG: usize = 50;

/// Environment-driven configuration for the bot.
#[derive(Clone, Deserialize)]
pub struct Config {
    /// The Discord bot token.
    pub discord_bot_token: String,

    /// The guild the bot operates in.
    pub guild_id: GuildId,

    /// The only Discord user allowed to run commands and talk to agents.
    /// When unset, everyone can.
    #[serde(default)]
    pub allowed_user_id: Option<UserId>,
}

impl Config {
    pub fn from_env() -> envy::Result<Self> {
        envy::from_env()
    }
}

/// The bot's state database directory: `$XDG_STATE_HOME/herdcord`, else
/// `~/.local/state/herdcord`.
#[must_use]
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("herdcord")
}

/// Path to the herdr Unix socket: `HERDR_SOCKET_PATH` when set, else
/// `herdr.sock` under the herdr config dir (`sessions/<name>/herdr.sock`
/// for a named session via `HERDR_SESSION`).
#[must_use]
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    std::env::var_os("HERDR_SESSION").map_or_else(
        || herdr_config_dir().join("herdr.sock"),
        |session| session_socket_path(&session.to_string_lossy()),
    )
}

/// The API socket of the named herdr session `name`, regardless of any
/// `HERDR_SOCKET_PATH` override.
#[must_use]
pub fn session_socket_path(name: &str) -> PathBuf {
    herdr_config_dir()
        .join("sessions")
        .join(name)
        .join("herdr.sock")
}

/// herdr's config directory (`$XDG_CONFIG_HOME/herdr`, else
/// `~/.config/herdr`): the parent of the main socket and the
/// `sessions/<name>/` trees.
fn herdr_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("herdr")
}

impl Debug for Config {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("guild_id", &self.guild_id)
            .field("allowed_user_id", &self.allowed_user_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::state_dir;

    #[test]
    fn state_dir_resolves_under_a_base_dir() {
        let path = state_dir();
        assert!(path.ends_with("herdcord"));
    }
}

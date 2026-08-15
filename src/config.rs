//! Minimal configuration: only what is deployment-specific is read from the
//! environment. Everything else is a sane-default const here; knobs can be
//! promoted to env vars later if needed.

use std::{
    fmt::{self, Debug, Formatter},
    path::{Path, PathBuf},
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

    /// The `/herdr` control command: a one-shot external command run with
    /// the user's prompt piped to its stdin (see [`crate::control`]).
    /// When unset, `/herdr` is not registered at all.
    #[serde(default)]
    pub herdr_control_command: Option<String>,

    /// Working directory for the control command; defaults to the home
    /// directory.
    #[serde(default)]
    pub herdr_control_cwd: Option<PathBuf>,

    /// How long one control command may run, in seconds; defaults to
    /// [`CONTROL_TIMEOUT`].
    #[serde(default)]
    pub herdr_control_timeout: Option<u64>,
}

impl Config {
    pub fn from_env() -> envy::Result<Self> {
        envy::from_env()
    }

    /// The control command's working directory: `HERDR_CONTROL_CWD` when
    /// set (a leading `~`/`~/` expands to the home directory), else the
    /// home directory (falling back to `/tmp`).
    #[must_use]
    pub fn control_cwd(&self) -> PathBuf {
        let home = dirs::home_dir();
        let Some(cwd) = self.herdr_control_cwd.clone() else {
            return home.unwrap_or_else(|| PathBuf::from("/tmp"));
        };
        if cwd == Path::new("~") {
            return home.unwrap_or(cwd);
        }
        if let Ok(rest) = cwd.strip_prefix("~/")
            && let Some(home) = home
        {
            return home.join(rest);
        }
        cwd
    }

    /// How long one control command may run: `HERDR_CONTROL_TIMEOUT` when
    /// set, else [`CONTROL_TIMEOUT`].
    #[must_use]
    pub fn control_timeout(&self) -> Duration {
        self.herdr_control_timeout
            .map_or(CONTROL_TIMEOUT, Duration::from_secs)
    }
}

/// How long one `/herdr` control command may run before its process
/// group is killed and the invocation reported as timed out.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(300);

/// Discord's per-message content cap; the control command's reply is
/// truncated to this.
pub const CONTROL_REPLY_LIMIT: usize = 2000;

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
    use std::path::PathBuf;

    use super::{CONTROL_TIMEOUT, state_dir};
    use crate::test_util::control_config;

    #[test]
    fn state_dir_resolves_under_a_base_dir() {
        let path = state_dir();
        assert!(path.ends_with("herdcord"));
    }

    #[test]
    fn control_timeout_defaults_to_300s() {
        let config = control_config(None, None, None);
        assert_eq!(config.control_timeout(), CONTROL_TIMEOUT);
    }

    #[test]
    fn control_timeout_honors_the_override() {
        let config = control_config(None, None, Some(42));
        assert_eq!(config.control_timeout(), std::time::Duration::from_secs(42));
    }

    #[test]
    fn control_cwd_defaults_to_the_home_directory() {
        let config = control_config(None, None, None);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        assert_eq!(config.control_cwd(), home);
    }

    #[test]
    fn control_cwd_honors_the_override() {
        let cwd = PathBuf::from("/some/control/dir");
        let config = control_config(None, Some(cwd.clone()), None);
        assert_eq!(config.control_cwd(), cwd);
    }

    #[test]
    fn control_cwd_expands_a_leading_tilde() {
        let config = control_config(None, Some(PathBuf::from("~/Projects")), None);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        assert_eq!(config.control_cwd(), home.join("Projects"));
    }
}

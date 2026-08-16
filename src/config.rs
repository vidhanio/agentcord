//! Configuration: a TOML file at `$XDG_CONFIG_HOME/herdcord/config.toml`
//! (else `~/.config/herdcord/config.toml`).
//!
//! Deployment-specific values and every delay knob live there; each field
//! has a sane default, documented by the consts below. The only
//! environment left is what herdr itself injects
//! (`HERDR_SOCKET_PATH`/`HERDR_SESSION`), honored as a fallback for the
//! socket resolution and as a dev override.
//!
//! String leaves support `${NAME}` environment-variable expansion. Expansion
//! happens after the `config` crate parses the file, so an environment value
//! cannot change the TOML structure or escaping. If `NAME` is not set, its
//! `${NAME}` placeholder is left unchanged.

use std::{
    fmt::{self, Debug, Formatter},
    path::{Path, PathBuf},
    time::Duration,
};

use config::{Config as Settings, File, FileFormat, Value, ValueKind};
use serde::Deserialize;
use serenity::all::{GuildId, UserId};

use crate::{error::BotError, session::Harness};

/// Default harness for agents launched from forum posts without a harness
/// tag.
pub const DEFAULT_HARNESS: Harness = Harness::Pi;

/// Timeout for individual herdr API calls.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the drift-backstop reconcile task runs; the event stream
/// handles updates instantly in between.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(600);

/// How often the transcript poll ticks: one stat per live session,
/// mirroring the changed ones and probing for rotations.
///
/// This is the reply-mirror cadence for the relay, which delivers prompts
/// without waiting on turns.
pub const MESSAGE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How many unsynced transcript messages one sync posts at most when a
/// backlog beyond [`CATCHUP_BACKLOG`] is dropped, announced in small
/// italic text so it can never flood the thread.
pub const MAX_SYNC_MESSAGES: usize = 5;

/// A sync counts as a catch-up (and truncates to the last
/// [`MAX_SYNC_MESSAGES`]) only when the unsynced backlog exceeds this.
/// Normal turns — even heavy tool-call turns — are mirrored whole.
pub const CATCHUP_BACKLOG: usize = 50;

/// How long one `/herdr` control command may run before its process
/// group is killed and the invocation reported as timed out.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(300);

/// Discord's per-message content cap; the control command's reply is
/// truncated to this.
pub const CONTROL_REPLY_LIMIT: usize = 2000;

/// How long a tracked transcript must stay unchanged before the poll
/// suspects the session rotated to a new file.
pub const SESSION_STALE_GRACE: Duration = Duration::from_secs(300);

/// How long a relay worker with no incoming messages stays alive.
pub const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Delay between attempts to (re)establish the herdr event subscription.
pub const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);

/// How long `agent.start` waits for the agent to be detected after the
/// placeholder response.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How often `agent.start` polls for detection.
pub const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long the `/agent` modal waits for the user to submit it.
pub const MODAL_TIMEOUT: Duration = Duration::from_secs(300);

/// The configurable delays: a `[delays]` table in the config file.
///
/// Each entry is an integer number of seconds or a human string such as
/// `"500ms"`, `"30s"`, `"5m"`, or `"1h"`. Missing entries fall back to
/// the consts above.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(default)]
pub struct Delays {
    /// Per herdr API call ([`OPERATION_TIMEOUT`]).
    #[serde(with = "duration_secs")]
    pub operation_timeout: Duration,

    /// Drift-backstop reconcile interval ([`SYNC_INTERVAL`]).
    #[serde(with = "duration_secs")]
    pub sync_interval: Duration,

    /// Transcript poll tick ([`MESSAGE_POLL_INTERVAL`]).
    #[serde(with = "duration_secs")]
    pub message_poll_interval: Duration,

    /// Transcript rotation staleness grace ([`SESSION_STALE_GRACE`]).
    #[serde(with = "duration_secs")]
    pub session_stale_grace: Duration,

    /// Per-agent relay worker lifetime ([`WORKER_IDLE_TIMEOUT`]).
    #[serde(with = "duration_secs")]
    pub relay_idle_timeout: Duration,

    /// Event-stream reconnect delay ([`RESUBSCRIBE_DELAY`]).
    #[serde(with = "duration_secs")]
    pub resubscribe_delay: Duration,

    /// `agent.start` detection wait ([`STARTUP_TIMEOUT`]).
    #[serde(with = "duration_secs")]
    pub agent_startup_timeout: Duration,

    /// `agent.start` detection poll ([`STARTUP_POLL_INTERVAL`]).
    #[serde(with = "duration_secs")]
    pub agent_startup_poll_interval: Duration,

    /// The `/agent` modal submission window ([`MODAL_TIMEOUT`]).
    #[serde(with = "duration_secs")]
    pub modal_timeout: Duration,
}

impl Default for Delays {
    fn default() -> Self {
        Self {
            operation_timeout: OPERATION_TIMEOUT,
            sync_interval: SYNC_INTERVAL,
            message_poll_interval: MESSAGE_POLL_INTERVAL,
            session_stale_grace: SESSION_STALE_GRACE,
            relay_idle_timeout: WORKER_IDLE_TIMEOUT,
            resubscribe_delay: RESUBSCRIBE_DELAY,
            agent_startup_timeout: STARTUP_TIMEOUT,
            agent_startup_poll_interval: STARTUP_POLL_INTERVAL,
            modal_timeout: MODAL_TIMEOUT,
        }
    }
}

/// TOML configuration for the bot, loaded from [`config_path`]: a
/// `[discord]` table, a `[herdr]` table, and the optional `[delays]`
/// table.
#[derive(Clone, Deserialize)]
pub struct Config {
    /// The Discord-facing configuration: token, guild, allowed user, and
    /// the mirror knobs.
    pub discord: Discord,

    /// The herdr-facing configuration: socket, default harness, and the
    /// `/herdr` control command.
    #[serde(default)]
    pub herdr: HerdrConfig,

    /// The delay knobs, all optional.
    #[serde(default)]
    pub delays: Delays,
}

/// The Discord-facing configuration: the bot token, the guild it operates
/// in, the single allowed user, and the transcript-mirror knobs.
#[derive(Clone, Debug, Deserialize)]
pub struct Discord {
    /// The Discord bot token.
    pub bot_token: String,

    /// The guild the bot operates in.
    pub guild_id: GuildId,

    /// The only Discord user allowed to run commands and talk to agents.
    pub allowed_user_id: UserId,

    /// The control command's reply cap ([`CONTROL_REPLY_LIMIT`]).
    #[serde(default = "control_reply_limit_default")]
    pub control_reply_limit: usize,

    /// How many unsynced transcript messages one sync posts at most during
    /// a catch-up ([`MAX_SYNC_MESSAGES`]).
    #[serde(default = "max_sync_messages_default")]
    pub max_sync_messages: usize,

    /// Backlog beyond which a sync counts as a catch-up
    /// ([`CATCHUP_BACKLOG`]).
    #[serde(default = "catchup_backlog_default")]
    pub catchup_backlog: usize,
}

/// The herdr-facing configuration: the socket, the default harness for
/// launched agents, and the `/herdr` control command.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct HerdrConfig {
    /// The herdr Unix socket; when unset, `HERDR_SOCKET_PATH` and
    /// `HERDR_SESSION` are honored, then the herdr config dir.
    pub socket_path: Option<PathBuf>,

    /// A named herdr session whose socket to use (ignored when
    /// `socket_path` is set).
    pub session: Option<String>,

    /// Harness preselected in the `/agent` modal and used to resume dead
    /// sessions whose post has no harness tag ([`DEFAULT_HARNESS`]).
    pub default_harness: Harness,

    /// The `/herdr` control command: a one-shot external command run with
    /// the user's prompt piped to its stdin (see [`crate::control`]).
    /// When unset, `/herdr` is not registered at all.
    pub control_command: Option<String>,

    /// Working directory for the control command; defaults to the home
    /// directory.
    pub control_cwd: Option<PathBuf>,

    /// How long one control command may run ([`CONTROL_TIMEOUT`]).
    #[serde(with = "duration_secs")]
    pub control_timeout: Duration,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            session: None,
            default_harness: DEFAULT_HARNESS,
            control_command: None,
            control_cwd: None,
            control_timeout: CONTROL_TIMEOUT,
        }
    }
}

impl Config {
    /// Loads the config from `path`, with a helpful error naming the
    /// path and a sample when the file is missing.
    ///
    /// The file is loaded through the `config` crate's synchronous builder.
    /// String leaves are expanded after parsing; see the module-level
    /// documentation for the supported environment syntax.
    ///
    /// # Errors
    ///
    /// Returns [`BotError::Other`] when the file cannot be read or is not
    /// valid TOML/configuration.
    pub fn load(path: &Path) -> Result<Self, BotError> {
        Self::from_source(File::from(path).format(FileFormat::Toml)).map_err(|error| {
            if path.exists() {
                BotError::Other(format!(
                    "invalid configuration at {}: {error}",
                    path.display()
                ))
            } else {
                BotError::Other(format!(
                    "no configuration at {} ({error}); create it with at least a \
                     `[discord]` table holding `bot_token`, `guild_id`, and \
                     `allowed_user_id`, e.g.\n{sample}",
                    path.display(),
                    sample = sample_config()
                ))
            }
        })
    }

    /// Parses a config from TOML text through the `config` crate builder.
    ///
    /// # Errors
    ///
    /// Returns the `config` crate error when the text is not valid config or
    /// does not contain the required fields.
    pub fn parse(raw: &str) -> Result<Self, config::ConfigError> {
        Self::from_source(File::from_str(raw, FileFormat::Toml))
    }

    fn from_source<S>(source: S) -> Result<Self, config::ConfigError>
    where
        S: config::Source + Send + Sync + 'static,
    {
        let mut settings = Settings::builder().add_source(source).build()?;
        expand_environment_values(&mut settings, &|name| std::env::var(name).ok());
        settings.try_deserialize()
    }
}

/// Expands environment placeholders in all string leaves of a parsed
/// configuration tree. The tree is already parsed by `config`, so replacement
/// values are data, not TOML source.
fn expand_environment_values<F>(settings: &mut Settings, lookup: &F)
where
    F: Fn(&str) -> Option<String>,
{
    expand_value(&mut settings.cache, lookup);
}

fn expand_value<F>(value: &mut Value, lookup: &F)
where
    F: Fn(&str) -> Option<String>,
{
    match &mut value.kind {
        ValueKind::String(text) => *text = expand_string(text, lookup),
        ValueKind::Table(table) => {
            for value in table.values_mut() {
                expand_value(value, lookup);
            }
        }
        ValueKind::Array(values) => {
            for value in values {
                expand_value(value, lookup);
            }
        }
        ValueKind::Nil
        | ValueKind::Boolean(_)
        | ValueKind::I64(_)
        | ValueKind::I128(_)
        | ValueKind::U64(_)
        | ValueKind::U128(_)
        | ValueKind::Float(_) => {}
    }
}

/// Expands `${NAME}` placeholders in one string. A single pass is
/// intentional: an environment value containing another placeholder is not
/// interpreted recursively.
fn expand_string<F>(value: &str, lookup: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut expanded = String::with_capacity(value.len());
    let mut remaining = value;

    while let Some(start) = remaining.find("${") {
        expanded.push_str(&remaining[..start]);
        let placeholder = &remaining[start..];
        let after_open = &placeholder[2..];
        let Some(end) = after_open.find('}') else {
            expanded.push_str(placeholder);
            return expanded;
        };

        let name = &after_open[..end];
        let token_len = 2 + end + 1;
        if is_environment_name(name) {
            if let Some(replacement) = lookup(name) {
                expanded.push_str(&replacement);
            } else {
                expanded.push_str(&placeholder[..token_len]);
            }
        } else {
            expanded.push_str(&placeholder[..token_len]);
        }
        remaining = &after_open[end + 1..];
    }

    expanded.push_str(remaining);
    expanded
}

fn is_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

impl HerdrConfig {
    /// The control command's working directory: `control_cwd` when set (a
    /// leading `~`/`~/` expands to the home directory), else the home
    /// directory (falling back to `/tmp`).
    #[must_use]
    pub fn control_cwd(&self) -> PathBuf {
        let home = dirs::home_dir();
        let Some(cwd) = self.control_cwd.clone() else {
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

    /// How long one control command may run: `control_timeout` when set,
    /// else [`CONTROL_TIMEOUT`].
    #[must_use]
    pub const fn control_timeout(&self) -> Duration {
        self.control_timeout
    }

    /// The herdr Unix socket: `socket_path` when set, else the
    /// herdr-injected or dev-set environment (`HERDR_SOCKET_PATH` /
    /// `HERDR_SESSION`), else `herdr.sock` under the herdr config dir.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.socket_path.clone().unwrap_or_else(default_socket_path)
    }
}

const fn control_reply_limit_default() -> usize {
    CONTROL_REPLY_LIMIT
}

const fn max_sync_messages_default() -> usize {
    MAX_SYNC_MESSAGES
}

const fn catchup_backlog_default() -> usize {
    CATCHUP_BACKLOG
}

/// The bot's configuration path: `$XDG_CONFIG_HOME/herdcord/config.toml`,
/// else `~/.config/herdcord/config.toml`.
#[must_use]
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("herdcord")
        .join("config.toml")
}

/// The bot's state database directory: `$XDG_STATE_HOME/herdcord`, else
/// `~/.local/state/herdcord`.
#[must_use]
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("herdcord")
}

/// Path to the herdr Unix socket from the environment: `HERDR_SOCKET_PATH`
/// when set, else `herdr.sock` under the herdr config dir.
///
/// herdr injects `HERDR_SOCKET_PATH` when the bot runs inside a herdr
/// session; the var is also honored as a dev override. `HERDR_SESSION`
/// selects a named session's socket (`sessions/<name>/herdr.sock`).
#[must_use]
pub fn default_socket_path() -> PathBuf {
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

/// A sample configuration with every knob documented, shown in the
/// missing-config error.
#[must_use]
pub const fn sample_config() -> &'static str {
    r#"[discord]
bot_token = "..."
guild_id = 1234567890
allowed_user_id = 1234567890
# control_reply_limit = 2000        # default: discord's message cap
# max_sync_messages = 5
# catchup_backlog = 50
# String values support `${NAME}` environment expansion. Missing variables
# remain as the literal `${NAME}` text.

[herdr]
# socket_path = "/path/to/herdr.sock"
# session = "main"                  # named herdr session, else the main socket
# default_harness = "pi"            # omp | claude-code | codex | pi | opencode
# control_command = "pi -p --no-session --tools bash --no-skills"
# control_cwd = "~"                 # default: home directory
# control_timeout = "5m"            # default: 300s

# delays: integer seconds or a string like "500ms", "30s", "5m", "1h"
[delays]
# operation_timeout = "30s"          # per herdr api call
# sync_interval = "10m"              # reconcile drift backstop
# message_poll_interval = "1s"       # transcript poll tick
# session_stale_grace = "5m"         # transcript rotation staleness
# relay_idle_timeout = "10m"         # per-agent relay worker lifetime
# resubscribe_delay = "5s"           # event-stream reconnect delay
# agent_startup_timeout = "30s"      # agent.start detection wait
# agent_startup_poll_interval = "100ms"
# modal_timeout = "5m"               # /agent modal submission window
"#
}

/// Deserializes a duration from an integer (seconds) or a human string
/// such as `"500ms"`, `"30s"`, `"5m"`, or `"1h"`.
mod duration_secs {
    use std::fmt;

    use serde::de::{Error, Visitor};

    use super::Duration;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = Duration;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer number of seconds or a duration string like \"5m\"")
            }

            fn visit_i64<E: Error>(self, seconds: i64) -> Result<Self::Value, E> {
                u64::try_from(seconds)
                    .map(Duration::from_secs)
                    .map_err(E::custom)
            }

            fn visit_u64<E: Error>(self, seconds: u64) -> Result<Self::Value, E> {
                Ok(Duration::from_secs(seconds))
            }

            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                humantime::parse_duration(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

impl Debug for Config {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("guild_id", &self.discord.guild_id)
            .field("allowed_user_id", &self.discord.allowed_user_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use config::{Config as Settings, File, FileFormat};

    use super::{
        CONTROL_TIMEOUT, Config, Delays, config_path, expand_environment_values, sample_config,
        state_dir,
    };
    use crate::test_util::control_config;

    #[test]
    fn state_dir_resolves_under_a_base_dir() {
        let path = state_dir();
        assert!(path.ends_with("herdcord"));
    }

    #[test]
    fn config_path_resolves_under_a_config_dir() {
        let path = config_path();
        assert!(path.ends_with("herdcord/config.toml"));
    }

    #[test]
    fn config_loads_from_a_file_with_the_builder() {
        static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdcord-config-test-{}-{}.toml",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &path,
            "[discord]\nbot_token = \"token\"\nguild_id = 7\nallowed_user_id = 8\n",
        )
        .expect("write test config");

        let config = Config::load(&path).expect("builder loads config");
        std::fs::remove_file(path).expect("remove test config");

        assert_eq!(config.discord.guild_id, serenity::all::GuildId::new(7));
        assert_eq!(
            config.discord.allowed_user_id,
            serenity::all::UserId::new(8)
        );
        assert_eq!(config.delays, Delays::default());
    }

    #[test]
    fn control_timeout_defaults_to_300s() {
        let config = control_config(None, None, None);
        assert_eq!(config.herdr.control_timeout(), CONTROL_TIMEOUT);
    }

    #[test]
    fn control_timeout_honors_the_override() {
        let config = control_config(None, None, Some(Duration::from_secs(42)));
        assert_eq!(config.herdr.control_timeout(), Duration::from_secs(42));
    }

    #[test]
    fn control_cwd_defaults_to_the_home_directory() {
        let config = control_config(None, None, None);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        assert_eq!(config.herdr.control_cwd(), home);
    }

    #[test]
    fn control_cwd_honors_the_override() {
        let cwd = PathBuf::from("/some/control/dir");
        let config = control_config(None, Some(cwd.clone()), None);
        assert_eq!(config.herdr.control_cwd(), cwd);
    }

    #[test]
    fn control_cwd_expands_a_leading_tilde() {
        let config = control_config(None, Some(PathBuf::from("~/Projects")), None);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        assert_eq!(config.herdr.control_cwd(), home.join("Projects"));
    }

    #[test]
    fn delays_default_to_the_documented_consts() {
        let defaults = Delays::default();
        assert_eq!(defaults.operation_timeout, super::OPERATION_TIMEOUT);
        assert_eq!(defaults.sync_interval, super::SYNC_INTERVAL);
        assert_eq!(defaults.message_poll_interval, super::MESSAGE_POLL_INTERVAL);
        assert_eq!(defaults.session_stale_grace, super::SESSION_STALE_GRACE);
        assert_eq!(defaults.relay_idle_timeout, super::WORKER_IDLE_TIMEOUT);
        assert_eq!(defaults.resubscribe_delay, super::RESUBSCRIBE_DELAY);
        assert_eq!(defaults.agent_startup_timeout, super::STARTUP_TIMEOUT);
        assert_eq!(
            defaults.agent_startup_poll_interval,
            super::STARTUP_POLL_INTERVAL
        );
        assert_eq!(defaults.modal_timeout, super::MODAL_TIMEOUT);
    }

    #[test]
    fn sample_config_parses_with_defaults() {
        let config = crate::config::Config::parse(sample_config()).expect("sample parses");
        assert_eq!(
            config.discord.guild_id,
            serenity::all::GuildId::new(1_234_567_890)
        );
        assert_eq!(
            config.discord.allowed_user_id,
            serenity::all::UserId::new(1_234_567_890)
        );
        assert_eq!(config.herdr.default_harness, crate::session::Harness::Pi);
        assert_eq!(config.delays, Delays::default());
    }

    #[test]
    fn environment_expansion_replaces_string_leaves_after_parsing() {
        let raw = r#"
            [discord]
            bot_token = "${BOT_TOKEN}"
            guild_id = 1
            allowed_user_id = 2

            [herdr]
            socket_path = "${SOCKET_PATH}"
            control_command = "run ${COMMAND}"
            control_timeout = "${TIMEOUT}"
            "#;
        let values = [
            ("BOT_TOKEN", "token with \"quotes\" and \\\"backslashes\\\""),
            ("SOCKET_PATH", "/run/herdr.sock"),
            ("COMMAND", "say \"hello\""),
            ("TIMEOUT", "2m"),
        ];
        let mut settings = Settings::builder()
            .add_source(File::from_str(raw, FileFormat::Toml))
            .build()
            .expect("builder parses config");
        expand_environment_values(&mut settings, &|name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        });
        let config: Config = settings
            .try_deserialize()
            .expect("expanded config deserializes");

        assert_eq!(config.discord.bot_token, values[0].1);
        assert_eq!(config.herdr.socket_path, Some(PathBuf::from(values[1].1)));
        assert_eq!(
            config.herdr.control_command.as_deref(),
            Some("run say \"hello\"")
        );
        assert_eq!(config.herdr.control_timeout, Duration::from_secs(120));
    }

    #[test]
    fn missing_environment_variables_remain_literal_and_are_not_recursive() {
        let expanded =
            super::expand_string("before-${MISSING}-${NESTED}-after", &|name| match name {
                "NESTED" => Some("${MISSING}".to_owned()),
                _ => None,
            });
        assert_eq!(expanded, "before-${MISSING}-${MISSING}-after");
    }

    #[test]
    fn minimal_config_parses_with_all_defaults() {
        let config = crate::config::Config::parse(
            "[discord]\nbot_token = \"token\"\nguild_id = 1\nallowed_user_id = 2\n",
        )
        .expect("minimal config parses");
        assert_eq!(
            config.discord.allowed_user_id,
            serenity::all::UserId::new(2)
        );
        assert_eq!(config.delays, Delays::default());
        assert_eq!(config.herdr.control_timeout(), CONTROL_TIMEOUT);
        assert_eq!(config.discord.control_reply_limit, 2000);
        assert_eq!(config.discord.max_sync_messages, 5);
        assert_eq!(config.discord.catchup_backlog, 50);
        assert!(config.herdr.control_command.is_none());
    }

    #[test]
    fn config_honors_duration_overrides() {
        let config = crate::config::Config::parse(
            r#"
            [discord]
            bot_token = "token"
            guild_id = 1
            allowed_user_id = 2
            control_reply_limit = 100
            max_sync_messages = 2
            catchup_backlog = 10

            [herdr]
            default_harness = "claude"
            control_timeout = 42

            [delays]
            operation_timeout = "500ms"
            sync_interval = "1h"
            message_poll_interval = 3
            session_stale_grace = "2m"
            relay_idle_timeout = "90s"
            resubscribe_delay = "1s"
            agent_startup_timeout = "15s"
            agent_startup_poll_interval = "50ms"
            modal_timeout = "2m"
            "#,
        )
        .expect("override config parses");
        assert_eq!(
            config.herdr.default_harness,
            crate::session::Harness::ClaudeCode
        );
        assert_eq!(config.herdr.control_timeout, Duration::from_secs(42));
        assert_eq!(config.discord.control_reply_limit, 100);
        assert_eq!(config.discord.max_sync_messages, 2);
        assert_eq!(config.discord.catchup_backlog, 10);
        assert_eq!(config.delays.operation_timeout, Duration::from_millis(500));
        assert_eq!(config.delays.sync_interval, Duration::from_secs(3600));
        assert_eq!(config.delays.message_poll_interval, Duration::from_secs(3));
        assert_eq!(config.delays.session_stale_grace, Duration::from_secs(120));
        assert_eq!(config.delays.relay_idle_timeout, Duration::from_secs(90));
        assert_eq!(config.delays.resubscribe_delay, Duration::from_secs(1));
        assert_eq!(config.delays.agent_startup_timeout, Duration::from_secs(15));
        assert_eq!(
            config.delays.agent_startup_poll_interval,
            Duration::from_millis(50)
        );
        assert_eq!(config.delays.modal_timeout, Duration::from_secs(120));
    }

    #[test]
    fn partial_delays_table_falls_back_per_field() {
        let config = crate::config::Config::parse(
            "[discord]\nbot_token = \"token\"\nguild_id = 1\nallowed_user_id = 2\n\n[delays]\nmessage_poll_interval = \"250ms\"\n",
        )
        .expect("partial delays parse");
        assert_eq!(
            config.delays.message_poll_interval,
            Duration::from_millis(250)
        );
        assert_eq!(config.delays.operation_timeout, super::OPERATION_TIMEOUT);
        assert_eq!(config.delays.sync_interval, super::SYNC_INTERVAL);
    }

    #[test]
    fn invalid_duration_is_rejected() {
        let result = crate::config::Config::parse(
            "[discord]\nbot_token = \"token\"\nguild_id = 1\nallowed_user_id = 2\n\n[herdr]\ncontrol_timeout = \"soon\"\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn unknown_harness_is_rejected() {
        let result = crate::config::Config::parse(
            "[discord]\nbot_token = \"token\"\nguild_id = 1\nallowed_user_id = 2\n\n[herdr]\ndefault_harness = \"bogus\"\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn missing_required_keys_are_rejected() {
        assert!(crate::config::Config::parse("[discord]\nguild_id = 1\n").is_err());
        assert!(crate::config::Config::parse("[discord]\nbot_token = \"token\"\n").is_err());
        assert!(
            crate::config::Config::parse("[discord]\nbot_token = \"token\"\nguild_id = 1\n")
                .is_err()
        );
        assert!(crate::config::Config::parse("bot_token = \"token\"\n").is_err());
    }
}

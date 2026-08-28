//! Agentcord's entirely configuration-driven runtime contract.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use config::{Config as Settings, File, FileFormat, Value, ValueKind};
use serde::Deserialize;
use serenity::all::{ChannelId, GuildId, UserId};

use crate::{BotError, BotResult};

/// Maximum tags Discord permits on a forum channel.
const DISCORD_FORUM_TAG_LIMIT: usize = 20;
/// Maximum options Discord permits in a select component.
const DISCORD_SELECT_LIMIT: usize = 25;

/// Complete configuration for one Agentcord process.
#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// Discord connection and projection settings.
    pub discord: DiscordConfig,
    /// Project path resolution settings.
    pub projects: ProjectsConfig,
    /// Configured ACP agents keyed by stable identifier.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Permission-response behavior.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// Operation time limits and render debounce interval.
    #[serde(default)]
    pub timeouts: Timeouts,
}

/// Discord account, guild, user, and forum identifiers.
#[derive(Clone, Debug, Deserialize)]
pub struct DiscordConfig {
    /// Bot token used to authenticate with Discord.
    pub bot_token: String,
    /// Guild containing the configured forum.
    pub guild_id: GuildId,
    /// Sole user allowed to control Agentcord.
    pub allowed_user_id: UserId,
    /// Forum where ACP sessions are projected as posts.
    pub forum_channel_id: ChannelId,
}

/// Filesystem settings used to resolve and label projects.
#[derive(Clone, Debug, Deserialize)]
pub struct ProjectsConfig {
    /// Base path used to shorten project display labels.
    pub base_path: PathBuf,
}

/// Command and presentation settings for one ACP agent.
#[derive(Clone, Debug, Deserialize)]
pub struct AgentConfig {
    /// Human-readable name shown in Discord.
    pub display_name: String,
    /// Executable spawned directly for ACP communication.
    pub command: PathBuf,
    /// Arguments passed directly to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment overrides passed to the subprocess.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Emoji used for the agent's forum tag.
    pub emoji: TagEmoji,
}

/// Global policy for ACP permission requests.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct PermissionsConfig {
    /// Whether every permission request should be approved automatically.
    #[serde(default)]
    pub approve_all: bool,
}

/// Unicode or custom Discord emoji used by an agent tag.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum TagEmoji {
    /// A Unicode emoji string.
    Unicode(String),
    /// A guild-specific custom emoji.
    Custom {
        /// Discord emoji snowflake.
        id: u64,
        /// Whether Discord should render the custom emoji as animated.
        #[serde(default)]
        animated: bool,
    },
}

/// Time limits for user interaction, ACP calls, and Discord edits.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    /// Time allowed for modal submission.
    #[serde(with = "duration")]
    pub modal: Duration,
    /// Time allowed for agent startup and bounded renderer drain.
    #[serde(with = "duration")]
    pub startup: Duration,
    /// Time allowed for an ACP prompt turn.
    #[serde(with = "duration")]
    pub prompt: Duration,
    /// Time allowed for a permission response.
    #[serde(with = "duration")]
    pub permission: Duration,
    /// Window used to batch adjacent ACP updates into fewer Discord edits.
    #[serde(with = "duration")]
    pub edit_debounce: Duration,
}

impl Default for Timeouts {
    /// Supplies conservative defaults for Discord and ACP operations.
    fn default() -> Self {
        Self {
            modal: Duration::from_secs(300),
            startup: Duration::from_secs(60),
            prompt: Duration::from_secs(3600),
            permission: Duration::from_secs(300),
            edit_debounce: Duration::from_millis(100),
        }
    }
}

impl Config {
    /// Loads, expands, and validates configuration from a TOML file.
    pub fn load(path: &Path) -> BotResult<Self> {
        let config =
            Self::from_source(File::from(path).format(FileFormat::Toml)).map_err(|error| {
                BotError::Config(format!("failed to load {}: {error}", path.display()))
            })?;
        config.validate()?;
        Ok(config)
    }

    /// Parses configuration from an in-memory TOML string.
    pub fn parse(raw: &str) -> Result<Self, config::ConfigError> {
        Self::from_source(File::from_str(raw, FileFormat::Toml))
    }

    /// Builds configuration from an arbitrary supported config source.
    fn from_source<S>(source: S) -> Result<Self, config::ConfigError>
    where
        S: config::Source + Send + Sync + 'static,
    {
        let mut settings = Settings::builder().add_source(source).build()?;
        expand_value(&mut settings.cache, &|name| std::env::var(name).ok());
        settings.try_deserialize()
    }

    /// Rejects invalid identifiers, paths, timeouts, and Discord settings.
    pub fn validate(&self) -> BotResult {
        if self.agents.is_empty() {
            return Err(BotError::Config(
                "at least one `[agents.<key>]` is required".into(),
            ));
        }
        if self.agents.len() > DISCORD_FORUM_TAG_LIMIT {
            return Err(BotError::Config(format!(
                "{} agents exceed Discord's {DISCORD_FORUM_TAG_LIMIT}-tag forum limit",
                self.agents.len()
            )));
        }
        if self.agents.len() > DISCORD_SELECT_LIMIT {
            return Err(BotError::Config(format!(
                "{} agents exceed Discord's {DISCORD_SELECT_LIMIT}-option selector limit",
                self.agents.len()
            )));
        }
        for (key, agent) in &self.agents {
            if key.trim().is_empty() || key.chars().count() > 20 {
                return Err(BotError::Config(format!(
                    "agent key `{key}` must contain 1–20 characters because it names the forum tag"
                )));
            }
            if agent.display_name.trim().is_empty() || agent.display_name.chars().count() > 100 {
                return Err(BotError::Config(format!(
                    "agent `{key}` display name must contain 1–100 characters"
                )));
            }
            if agent.command.as_os_str().is_empty() {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an empty command"
                )));
            }
            if matches!(&agent.emoji, TagEmoji::Unicode(value) if value.trim().is_empty()) {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an empty tag emoji"
                )));
            }
            if matches!(agent.emoji, TagEmoji::Custom { id: 0, .. }) {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an invalid custom emoji id"
                )));
            }
        }
        Ok(())
    }
}

#[must_use]
/// Returns the user-specific Agentcord configuration path.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("agentcord/config.toml")
}

#[must_use]
/// Returns the user-specific SQLite state path.
pub fn state_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("agentcord/state.sqlite3")
}

/// Recursively expands environment references in configuration values.
fn expand_value<F>(value: &mut Value, lookup: &F)
where
    F: Fn(&str) -> Option<String>,
{
    match &mut value.kind {
        ValueKind::String(text) => *text = expand_string(text, lookup),
        ValueKind::Table(table) => {
            for child in table.values_mut() {
                expand_value(child, lookup);
            }
        }
        ValueKind::Array(values) => {
            for child in values {
                expand_value(child, lookup);
            }
        }
        _ => {}
    }
}

/// Expands `${NAME}` references in one string.
fn expand_string<F>(value: &str, lookup: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let placeholder = &rest[start..];
        let Some(end) = placeholder[2..].find('}') else {
            output.push_str(placeholder);
            return output;
        };
        let name = &placeholder[2..end + 2];
        let token_len = end + 3;
        if valid_env_name(name) {
            output.push_str(&lookup(name).unwrap_or_else(|| placeholder[..token_len].to_owned()));
        } else {
            output.push_str(&placeholder[..token_len]);
        }
        rest = &placeholder[token_len..];
    }
    output.push_str(rest);
    output
}

/// Validates a candidate environment-variable name.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('_' | 'A'..='Z' | 'a'..='z'))
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

mod duration {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    /// Accepted serialized duration representations.
    enum Repr {
        /// Whole seconds.
        Seconds(u64),
        /// A value parsed by `humantime`.
        Human(String),
    }

    /// Deserializes either seconds or a human-readable duration.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Repr::deserialize(deserializer)? {
            Repr::Seconds(value) => Ok(Duration::from_secs(value)),
            Repr::Human(value) => {
                humantime::parse_duration(&value).map_err(serde::de::Error::custom)
            }
        }
    }
}

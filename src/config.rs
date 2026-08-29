//! Agentcord's configuration contract.
//!
//! Configuration is loaded from TOML after expanding `${NAME}` references in
//! string values. Expansion is performed on the parsed value tree, so an
//! environment value is data and cannot change the TOML structure.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use config::{Config as Settings, File, FileFormat, Value, ValueKind};
use nutype::nutype;
use serde::Deserialize;
use serenity::all::{ChannelId, EmojiId, GuildId, UserId};

use crate::{BotError, BotResult};

/// Maximum tags Discord permits on a forum channel.
const DISCORD_FORUM_TAG_LIMIT: usize = 20;
/// Maximum options Discord permits in a select component.
const DISCORD_SELECT_LIMIT: usize = 25;

/// Stable configuration key identifying one ACP agent.
#[nutype(derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Display,
    AsRef,
    Deref,
    Borrow,
    From,
    Serialize,
    Deserialize
))]
pub struct AgentKey(
    /// Stable text key used to select the configured agent.
    String,
);

/// Complete configuration for one Agentcord process.
#[derive(Clone, Deserialize)]
pub struct Config {
    /// Discord connection and projection settings.
    pub discord: DiscordConfig,
    /// Project path resolution settings.
    pub projects: ProjectsConfig,
    /// Configured ACP agents keyed by stable identifier.
    pub agents: BTreeMap<AgentKey, AgentConfig>,
    /// Permission-response behavior.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// Operation time limits and render debounce interval.
    #[serde(default)]
    pub timeouts: Timeouts,
}

/// Discord account, guild, user, and forum identifiers.
#[derive(Clone, Deserialize)]
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
#[derive(Clone, Deserialize)]
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
        id: EmojiId,
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
    ///
    /// Parsing and validation are separate so callers that build a config in
    /// more than one source can validate only after merging those sources.
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

    /// Rejects invalid identifiers and values that cannot be represented by
    /// Agentcord's Discord surface.
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
            if matches!(agent.emoji, TagEmoji::Custom { id, .. } if id.get() == 0) {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an invalid custom emoji id"
                )));
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Config {
    /// Formats useful configuration metadata without exposing secrets.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("discord", &self.discord)
            .field("projects", &self.projects)
            .field("agents", &self.agents)
            .field("permissions", &self.permissions)
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl fmt::Debug for DiscordConfig {
    /// Formats Discord identifiers while redacting the bot token.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordConfig")
            .field("bot_token", &"[REDACTED]")
            .field("guild_id", &self.guild_id)
            .field("allowed_user_id", &self.allowed_user_id)
            .field("forum_channel_id", &self.forum_channel_id)
            .finish()
    }
}

impl fmt::Debug for AgentConfig {
    /// Formats agent settings without exposing arbitrary environment values.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentConfig")
            .field("display_name", &self.display_name)
            .field("command", &self.command)
            .field("args", &"[REDACTED]")
            .field("env", &"[REDACTED]")
            .field("emoji", &self.emoji)
            .finish()
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
///
/// A single pass is intentional: if an environment value contains another
/// placeholder, that value is not interpreted as a second template.
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

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use config::{Config as Settings, File, FileFormat};

    use super::{AgentKey, Config, Timeouts, config_path, expand_string, expand_value, state_path};

    /// Returns the smallest valid configuration used by parser tests.
    fn minimal_config() -> &'static str {
        r#"
            [discord]
            bot_token = "token"
            guild_id = 1
            allowed_user_id = 2
            forum_channel_id = 3

            [projects]
            base_path = "~/Projects"

            [agents.example]
            display_name = "Example Agent"
            command = "example-agent-acp"
            emoji = "🤖"
        "#
    }

    /// Verifies defaults and both supported duration formats.
    #[test]
    fn parses_schema_defaults_and_duration_forms() {
        let config = Config::parse(
            r#"
                [discord]
                bot_token = "token"
                guild_id = 1
                allowed_user_id = 2
                forum_channel_id = 3

                [projects]
                base_path = "/tmp/projects"

                [agents.example]
                display_name = "Example Agent"
                command = "example-agent-acp"
                args = ["--stdio"]
                env = { SETTING = "1" }
                emoji = { id = 7, animated = true }

                [timeouts]
                startup = 42
                prompt = "2m"
                edit_debounce = "100ms"
            "#,
        )
        .expect("valid configuration");

        assert_eq!(config.discord.guild_id, serenity::all::GuildId::new(1));
        assert_eq!(
            config.agents[&AgentKey::new("example")].args,
            vec![String::from("--stdio")]
        );
        assert_eq!(config.timeouts.startup, Duration::from_secs(42));
        assert_eq!(config.timeouts.prompt, Duration::from_secs(120));
        assert_eq!(config.timeouts.edit_debounce, Duration::from_millis(100));
        assert_eq!(config.timeouts.modal, Timeouts::default().modal);
        config.validate().expect("valid configuration");
    }

    /// Verifies environment expansion traverses nested values.
    #[test]
    fn expands_string_leaves_in_nested_tables_and_arrays() {
        let raw = r#"
            [nested]
            value = "prefix-${VALUE}"
            values = ["${VALUE}", "${MISSING}"]
            [nested.more]
            value = "${VALUE}/suffix"
        "#;
        let mut settings = Settings::builder()
            .add_source(File::from_str(raw, FileFormat::Toml))
            .build()
            .expect("valid TOML");
        expand_value(&mut settings.cache, &|name| match name {
            "VALUE" => Some("replacement".into()),
            _ => None,
        });
        let value: serde_json::Value = settings.try_deserialize().expect("expanded value tree");

        assert_eq!(value["nested"]["value"], "prefix-replacement");
        assert_eq!(value["nested"]["values"][0], "replacement");
        assert_eq!(value["nested"]["values"][1], "${MISSING}");
        assert_eq!(value["nested"]["more"]["value"], "replacement/suffix");
    }

    /// Verifies expansion is single-pass and preserves invalid placeholders.
    #[test]
    fn expansion_is_single_pass_and_leaves_invalid_placeholders() {
        let expanded = expand_string("${NESTED}-${bad}-${UNFINISHED", &|name| match name {
            "NESTED" => Some("${OTHER}".into()),
            _ => None,
        });
        assert_eq!(expanded, "${OTHER}-${bad}-${UNFINISHED");
    }

    /// Verifies validation rejects configurations without agents.
    #[test]
    fn validation_rejects_missing_agents() {
        let mut config = Config::parse(minimal_config()).expect("valid configuration");
        config.agents.clear();
        assert!(config.validate().is_err());
    }

    /// Verifies debug output redacts secret configuration values.
    #[test]
    fn debug_redacts_discord_and_agent_environment_secrets() {
        let mut config = Config::parse(minimal_config()).expect("valid configuration");
        config.discord.bot_token = "discord-secret".into();
        config
            .agents
            .get_mut(&AgentKey::new("example"))
            .expect("agent")
            .args = vec![String::from("agent-secret")];
        config
            .agents
            .get_mut(&AgentKey::new("example"))
            .expect("agent")
            .env = BTreeMap::from([(String::from("SECRET"), String::from("agent-secret"))]);

        let debug = format!("{config:?}");
        assert!(!debug.contains("discord-secret"));
        assert!(!debug.contains("agent-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    /// Verifies configuration and state paths stay under their application
    /// directories.
    #[test]
    fn paths_are_scoped_to_agentcord() {
        assert!(config_path().ends_with("agentcord/config.toml"));
        assert!(state_path().ends_with("agentcord/state.sqlite3"));
    }

    /// Verifies loading a TOML file performs parsing and validation.
    #[test]
    fn load_reads_and_validates_a_toml_file() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agentcord-config-{}-{suffix}.toml",
            std::process::id()
        ));
        fs::write(&path, minimal_config()).expect("write config");
        let result = Config::load(&path);
        fs::remove_file(&path).expect("remove config");
        result.expect("load valid config");
    }
}

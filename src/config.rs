//! Agentcord's entirely configuration-driven runtime contract.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use config::{Config as Settings, File, FileFormat, Value, ValueKind};
use serde::Deserialize;
use serenity::all::{ChannelId, GuildId, UserId};

use crate::{BotError, BotResult};

const DISCORD_FORUM_TAG_LIMIT: usize = 20;
const DISCORD_SELECT_LIMIT: usize = 25;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    pub projects: ProjectsConfig,
    pub agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub timeouts: Timeouts,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub guild_id: GuildId,
    pub allowed_user_id: UserId,
    pub forum_channel_id: ChannelId,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectsConfig {
    pub base_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentConfig {
    pub display_name: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub tag: AgentTag,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentTag {
    pub name: String,
    pub emoji: TagEmoji,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum TagEmoji {
    Unicode(String),
    Custom {
        id: u64,
        #[serde(default)]
        animated: bool,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    #[serde(with = "duration")]
    pub modal: Duration,
    #[serde(with = "duration")]
    pub startup: Duration,
    #[serde(with = "duration")]
    pub prompt: Duration,
    #[serde(with = "duration")]
    pub permission: Duration,
    #[serde(with = "duration")]
    pub edit_debounce: Duration,
}

impl Default for Timeouts {
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
    pub fn load(path: &Path) -> BotResult<Self> {
        let config =
            Self::from_source(File::from(path).format(FileFormat::Toml)).map_err(|error| {
                BotError::Config(format!("failed to load {}: {error}", path.display()))
            })?;
        config.validate()?;
        Ok(config)
    }

    pub fn parse(raw: &str) -> Result<Self, config::ConfigError> {
        Self::from_source(File::from_str(raw, FileFormat::Toml))
    }

    fn from_source<S>(source: S) -> Result<Self, config::ConfigError>
    where
        S: config::Source + Send + Sync + 'static,
    {
        let mut settings = Settings::builder().add_source(source).build()?;
        expand_value(&mut settings.cache, &|name| std::env::var(name).ok());
        settings.try_deserialize()
    }

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
        let mut tag_names = HashSet::new();
        for (key, agent) in &self.agents {
            if key.trim().is_empty() || key.len() > 100 {
                return Err(BotError::Config(format!(
                    "agent key `{key}` must contain 1–100 bytes"
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
            if agent.tag.name.trim().is_empty() || agent.tag.name.chars().count() > 20 {
                return Err(BotError::Config(format!(
                    "agent `{key}` tag name must contain 1–20 characters"
                )));
            }
            if !tag_names.insert(agent.tag.name.as_str()) {
                return Err(BotError::Config(format!(
                    "agent tag name `{}` is configured more than once",
                    agent.tag.name
                )));
            }
            if matches!(&agent.tag.emoji, TagEmoji::Unicode(value) if value.trim().is_empty()) {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an empty tag emoji"
                )));
            }
            if matches!(agent.tag.emoji, TagEmoji::Custom { id: 0, .. }) {
                return Err(BotError::Config(format!(
                    "agent `{key}` has an invalid custom emoji id"
                )));
            }
        }
        Ok(())
    }
}

#[must_use]
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("agentcord/config.toml")
}

#[must_use]
pub fn state_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from(".local/state"))
        .join("agentcord/state.sqlite3")
}

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
    enum Repr {
        Seconds(u64),
        Human(String),
    }

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

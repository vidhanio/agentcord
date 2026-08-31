//! Error types shared by Agentcord's boundaries.

use std::{num::ParseIntError, path::PathBuf};

use serenity::{all::GenericChannelId, model::mention::Mentionable};

/// Errors returned when parsing a `model[:reasoning]` selector.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ModelSpecError {
    /// The selector contains more than one reasoning separator.
    #[error("expected at most one reasoning separator")]
    ExtraSeparator {
        /// User-supplied selector.
        input: String,
    },
    /// One selector component is empty.
    #[error("model and reasoning must not be empty")]
    EmptyPart {
        /// User-supplied selector.
        input: String,
    },
    /// A selector component contains whitespace.
    #[error("provider, model, and reasoning cannot contain whitespace")]
    Whitespace {
        /// User-supplied selector.
        input: String,
    },
}

/// Result type used by Agentcord operations.
pub type BotResult<T = ()> = Result<T, BotError>;

/// Errors produced while constructing and using Agentcord.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// Configuration parsing failed for a file.
    #[error("failed to load configuration `{path}`: {source}")]
    ConfigLoad {
        /// Configuration file path.
        path: PathBuf,
        /// Underlying parser or source error.
        #[source]
        source: config::ConfigError,
    },
    /// No ACP agents were configured.
    #[error("at least one configured agent is required")]
    NoAgents,
    /// The configured agents exceed a Discord surface limit.
    #[error("{count} configured agents exceed discord's {limit}-tag forum limit")]
    TooManyAgents {
        /// Number of configured agents.
        count: usize,
        /// Maximum supported items.
        limit: usize,
    },
    /// A configured agent key cannot be used as a forum tag.
    #[error("agent key `{key}` must contain 1–20 characters")]
    InvalidAgentKey {
        /// Invalid configured key.
        key: String,
    },
    /// A configured agent display name cannot be shown in Discord.
    #[error("agent `{key}` display name must contain 1–100 characters")]
    InvalidAgentDisplayName {
        /// Configured agent key.
        key: String,
    },
    /// A configured agent has no executable command.
    #[error("agent `{key}` has an empty command")]
    EmptyAgentCommand {
        /// Configured agent key.
        key: String,
    },
    /// A configured agent has no tag emoji.
    #[error("agent `{key}` has an empty tag emoji")]
    EmptyAgentEmoji {
        /// Configured agent key.
        key: String,
    },
    /// A configured custom emoji ID is invalid.
    #[error("agent `{key}` has an invalid custom emoji id")]
    InvalidAgentEmojiId {
        /// Configured agent key.
        key: String,
    },
    /// An agent does not have a corresponding forum tag.
    #[error("missing forum tag for `{agent_key}`")]
    MissingForumTag {
        /// Configured agent key.
        agent_key: String,
    },
    /// The configured Discord channel is not a forum.
    #[error("discord channel `{channel}` is not a forum")]
    ForumChannelRequired {
        /// Configured channel identifier.
        channel: String,
    },
    /// The state database could not be read or updated.
    #[error(transparent)]
    Database(#[from] toasty::Error),
    /// A project path stored in the database is relative.
    #[error("acp project path `{path}` must be absolute")]
    RelativeProjectPath {
        /// Invalid project path.
        path: PathBuf,
    },
    /// A project path cannot be represented as UTF-8 for SQLite.
    #[error("project path `{path}` is not valid utf-8")]
    ProjectPathNotUtf8 {
        /// Invalid project path.
        path: PathBuf,
    },
    /// A database path cannot be represented as UTF-8.
    #[error("database path `{path}` is not valid utf-8")]
    DatabasePathNotUtf8 {
        /// Invalid database path.
        path: PathBuf,
    },
    /// A persisted Discord snowflake is malformed.
    #[error("invalid stored discord id `{value}`: {source}")]
    InvalidStoredDiscordId {
        /// Stored textual snowflake.
        value: String,
        /// Underlying integer parser error.
        #[source]
        source: ParseIntError,
    },
    /// A projection contains more messages than can be indexed.
    #[error("render projection contains too many messages")]
    ProjectionTooLarge,
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A project path could not be canonicalized.
    #[error("could not resolve {description} `{path}`: {source}")]
    ProjectPathResolution {
        /// Path supplied by the user.
        path: PathBuf,
        /// Human-readable path kind.
        description: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A resolved project path is not a directory.
    #[error("project path `{path}` is not a directory")]
    ProjectNotDirectory {
        /// Resolved path.
        path: PathBuf,
    },
    /// The current user's home directory could not be found.
    #[error("could not determine home directory")]
    HomeDirectoryUnavailable,
    /// A model selector could not be parsed.
    #[error(transparent)]
    Model(#[from] ModelSpecError),
    /// A configured agent key was not found.
    #[error("unknown agent `{key}`")]
    UnknownAgent {
        /// Missing configured agent key.
        key: String,
    },
    /// A command targeted a channel that is not an Agentcord session.
    #[error(
        "this is not an acp session in thread {mention}",
        mention = .thread.mention()
    )]
    NotSession {
        /// Discord thread supplied by the command or message handler.
        thread: GenericChannelId,
    },
    /// A session identifier was omitted or empty.
    #[error("the session id is empty")]
    EmptySessionId,
    /// A session is already bound to a live Discord thread.
    #[error(
        "this session is already imported in thread {mention}",
        mention = .thread.mention()
    )]
    AlreadyImported {
        /// Existing Discord thread.
        thread: GenericChannelId,
    },
    /// An imported agent session reported a relative project path.
    #[error("agent reported a non-absolute project path `{path}`")]
    NonAbsoluteProjectPath {
        /// Project path reported by the agent.
        path: PathBuf,
    },
    /// An ACP agent did not return the requested external session.
    #[error("agent does not know session `{session_id}`")]
    SessionNotFound {
        /// Requested ACP session identifier.
        session_id: String,
    },
    /// The ACP subprocess or protocol connection failed.
    #[error("acp error: {0}")]
    AcpProtocol(String),
    /// The bounded ACP command queue cannot accept another prompt or command.
    #[error("the acp prompt queue is full")]
    AcpQueueFull,
    /// The ACP session actor exited before accepting a command.
    #[error("the acp session actor has exited")]
    AcpActorExited,
    /// The existing actor did not stop within the configured startup timeout.
    #[error("acp session actor stop timed out")]
    AcpActorStopTimedOut,
    /// The ACP model-selection request did not complete in time.
    #[error("acp model selection timed out")]
    AcpModelSelectionTimedOut,
    /// Discord has not delivered its first ready event.
    #[error("discord is not ready")]
    DiscordNotReady,
    /// Renderer JSON state could not be decoded.
    #[error("projection {context} could not be decoded: {source}")]
    ProjectionDeserialize {
        /// State kind being decoded.
        context: &'static str,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Renderer state could not be encoded.
    #[error("projection {context} could not be encoded: {source}")]
    ProjectionSerialize {
        /// State kind being encoded.
        context: &'static str,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Renderer state was valid JSON but not an object.
    #[error("projection state must contain a json object")]
    ProjectionStateNotObject,
    /// A Discord API operation failed.
    #[error("discord error: {0}")]
    Serenity(Box<serenity::Error>),
    /// The configured bot token could not be parsed.
    #[error("invalid discord bot token: {message}")]
    InvalidDiscordToken {
        /// Parser-provided token error message.
        message: String,
    },
}

impl From<serenity::Error> for BotError {
    /// Wraps a Serenity error without exposing it as a public dependency type.
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

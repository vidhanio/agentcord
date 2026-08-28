use thiserror::Error;

/// Errors surfaced by configuration, ACP, persistence, Discord, or I/O work.
#[derive(Debug, Error)]
pub enum BotError {
    /// Invalid or unusable configuration.
    #[error("configuration error: {0}")]
    Config(String),
    /// ACP protocol or transport failure.
    #[error("ACP error: {0}")]
    Acp(#[from] agent_client_protocol::Error),
    /// SQLite operation failure.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// Discord API or gateway failure.
    #[error("Discord error: {0}")]
    Serenity(Box<serenity::Error>),
    /// Filesystem or process I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Domain failure without a more specific source type.
    #[error("{0}")]
    Other(String),
}

impl From<serenity::Error> for BotError {
    /// Preserves Serenity errors as the bot's Discord error variant.
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

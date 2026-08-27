use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("ACP error: {0}")]
    Acp(#[from] agent_client_protocol::Error),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Discord error: {0}")]
    Serenity(Box<serenity::Error>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl From<serenity::Error> for BotError {
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

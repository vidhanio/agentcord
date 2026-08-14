use thiserror::Error;

/// The bot's unified error type.
#[derive(Debug, Error)]
pub enum BotError {
    #[error(transparent)]
    Herdr(#[from] crate::herdr::Error),

    #[error(transparent)]
    Serenity(Box<serenity::Error>),

    #[error(transparent)]
    Toasty(#[from] toasty::Error),

    #[error("forum channel not found")]
    ForumChannelNotFound,

    #[error("{0}")]
    Other(String),
}

impl From<serenity::Error> for BotError {
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

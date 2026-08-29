//! Discord projection and supervision for Agent Client Protocol agents.

use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use agent_client_protocol::schema::v1::SessionId;
use serenity::all::{
    Context, EventHandler, FullEvent, GatewayIntents, HttpBuilder, Token, async_trait,
};
use tracing::warn;

mod acp;
mod commands;
mod forum;
mod projects;
pub mod render;
mod webhook;

pub mod config;
pub mod db;

pub use config::Config;
pub use db::Db;

/// Result type used by Agentcord operations.
pub type BotResult<T = ()> = Result<T, BotError>;

/// Identifies whether a prompt already has a visible Discord message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptOrigin {
    /// The gateway message is already visible and must not be mirrored.
    AlreadyVisible,
    /// The prompt came from another surface and needs a webhook mirror.
    NeedsMirror,
}

/// Errors produced while constructing and using Agentcord.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// Configuration could not be loaded or validated.
    #[error("configuration error: {0}")]
    Config(String),
    /// The state database could not be read or updated.
    #[error(transparent)]
    Database(#[from] toasty::Error),
    /// A database or persisted project path cannot be represented safely.
    #[error("database path error: {0}")]
    DatabasePath(String),
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A user supplied command argument is invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The ACP subprocess or protocol connection failed.
    #[error("ACP error: {0}")]
    Acp(String),
    /// Discord has not delivered its first ready event.
    #[error("Discord is not ready")]
    DiscordNotReady,
    /// A protocol update could not be reduced to renderer state.
    #[error("projection error: {0}")]
    Projection(String),
    /// A Discord API operation failed.
    #[error("Discord error: {0}")]
    Serenity(Box<serenity::Error>),
}

impl From<serenity::Error> for BotError {
    /// Wraps a Serenity error without exposing it as a public dependency type.
    fn from(error: serenity::Error) -> Self {
        Self::Serenity(Box::new(error))
    }
}

/// Cheaply cloneable handle to the process-wide application state.
#[derive(Clone)]
pub struct Bot(
    /// Shared application state used by every event and command task.
    Arc<BotState>,
);

/// Durable dependencies shared by Agentcord tasks.
pub struct BotState {
    /// Immutable validated configuration.
    pub config: Config,
    /// Persistent session and Discord projection state.
    pub db: Db,
    /// Serenity context installed by the first ready event.
    context: OnceLock<Context>,
    /// Cached webhook used to represent prompts as the allowed user.
    webhook: tokio::sync::Mutex<Option<serenity::all::Webhook>>,
    /// Per-thread ACP session actors.
    supervisor: acp::Supervisor,
}

impl Bot {
    /// Constructs application state using the configured state path.
    pub async fn new(config: Config) -> BotResult<Self> {
        config.validate()?;
        let db = Db::open(&config::state_path()).await?;
        Ok(Self::with_db(config, db))
    }

    /// Constructs application state with an already-open database.
    #[must_use]
    pub fn with_db(config: Config, db: Db) -> Self {
        Self(Arc::new(BotState {
            config,
            db,
            context: OnceLock::new(),
            webhook: tokio::sync::Mutex::new(None),
            supervisor: acp::Supervisor::default(),
        }))
    }

    /// Installs the Serenity context from the first ready event.
    pub fn install_context(&self, context: Context) {
        let _ = self.0.context.set(context);
    }

    /// Returns the installed Serenity context.
    pub fn context(&self) -> BotResult<&Context> {
        self.0.context.get().ok_or(BotError::DiscordNotReady)
    }

    /// Returns the immutable application configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.0.config
    }

    /// Returns the persistent state database.
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.0.db
    }

    /// Returns the shared application state for crate modules.
    pub(crate) fn state(&self) -> &BotState {
        &self.0
    }

    /// Forwards one prompt to the persisted session for a Discord thread.
    pub async fn forward_prompt(
        &self,
        thread: serenity::all::GenericChannelId,
        text: String,
        turn_id: String,
        origin: PromptOrigin,
    ) -> BotResult {
        let row = self
            .db()
            .session(thread)
            .await?
            .ok_or_else(|| BotError::Acp("this thread is not an ACP session".into()))?;
        self.state()
            .supervisor
            .prompt(self, &row, text, turn_id, origin)
    }

    /// Creates a new ACP session, binds it to a forum post, and queues its
    /// initial prompt.
    pub async fn create_session(
        &self,
        agent_key: &config::AgentKey,
        project: projects::Project,
        prompt: String,
    ) -> BotResult<serenity::all::GenericChannelId> {
        if prompt.trim().is_empty() {
            return Err(BotError::InvalidRequest("the prompt is empty".into()));
        }
        let created = self
            .state()
            .supervisor
            .new_session(self, agent_key, project.path.clone())
            .await?;
        let metadata = forum::SessionMetadata {
            agent_key: agent_key.clone(),
            project_label: project.label,
            project_path: project.path,
            session_id: created.session_id,
            title: None,
        };
        let row = self.create_session_post(&metadata).await?;
        if let Err(error) = self.db().insert_session(&row).await {
            let context = self.context()?.clone();
            let _ = row.thread_id.delete(&context.http, None).await;
            return Err(error);
        }
        self.forward_prompt(
            row.thread_id,
            prompt,
            row.thread_id.to_string(),
            PromptOrigin::NeedsMirror,
        )
        .await?;
        Ok(row.thread_id)
    }

    /// Imports an ACP session exposed by `session/list` into a forum post.
    pub async fn import_session(
        &self,
        agent_key: &config::AgentKey,
        session_id: &SessionId,
    ) -> BotResult<serenity::all::GenericChannelId> {
        if session_id.to_string().trim().is_empty() {
            return Err(BotError::InvalidRequest("the session id is empty".into()));
        }
        if let Some(existing) = self.db().session_by_agent(agent_key, session_id).await? {
            return Err(BotError::InvalidRequest(format!(
                "this session is already imported in thread {}",
                existing.thread_id
            )));
        }
        let imported = self
            .state()
            .supervisor
            .inspect_session(self, agent_key, session_id)
            .await?;
        if !imported.project_path.is_absolute() {
            return Err(BotError::InvalidRequest(format!(
                "agent reported a non-absolute project path `{}`",
                imported.project_path.display()
            )));
        }
        let project = projects::Project::adopt(&self.config().projects, &imported.project_path);
        let metadata = forum::SessionMetadata {
            agent_key: agent_key.clone(),
            project_label: project.label,
            project_path: project.path,
            session_id: imported.session_id,
            title: imported.title,
        };
        let row = self.create_session_post(&metadata).await?;
        if let Err(error) = self.db().insert_session(&row).await {
            let context = self.context()?.clone();
            let _ = row.thread_id.delete(&context.http, None).await;
            return Err(error);
        }
        Ok(row.thread_id)
    }

    /// Lists externally visible ACP sessions for command autocomplete.
    pub async fn list_sessions(
        &self,
        agent_key: &config::AgentKey,
    ) -> BotResult<Vec<acp::ListedSession>> {
        self.state().supervisor.list_sessions(self, agent_key).await
    }
}

impl fmt::Debug for Bot {
    /// Formats stable state without leaking runtime handles or secrets.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bot")
            .field("config", &self.0.config)
            .field("db", &self.0.db)
            .field("discord_ready", &self.0.context.get().is_some())
            .finish()
    }
}

impl fmt::Debug for BotState {
    /// Formats stable dependencies while omitting synchronization internals.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BotState")
            .field("config", &self.config)
            .field("db", &self.db)
            .field("discord_ready", &self.context.get().is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EventHandler for Bot {
    /// Installs the Discord context and dispatches message events.
    async fn dispatch(&self, context: &Context, event: &FullEvent) {
        let _ = self.0.context.set(context.clone());
        if let FullEvent::Message { new_message, .. } = event
            && let Err(error) = self.handle_message(new_message).await
        {
            warn!(?error, "failed to handle Discord message");
        }
    }
}

impl Bot {
    /// Handles one gateway message that may be a user prompt.
    async fn handle_message(&self, message: &serenity::all::Message) -> BotResult {
        let context = self.context()?.clone();
        if message.author.id != self.config().discord.allowed_user_id
            || message.author.id == context.cache.current_user().id
            || message.content.trim().is_empty()
        {
            return Ok(());
        }
        let Some(_) = self.db().session(message.channel_id).await? else {
            return Ok(());
        };
        if let Err(error) = self
            .forward_prompt(
                message.channel_id,
                message.content.to_string(),
                message.id.to_string(),
                PromptOrigin::AlreadyVisible,
            )
            .await
        {
            let _ = message
                .reply(&context.http, format!("ACP prompt failed: {error}"))
                .await;
        }
        Ok(())
    }
}

/// Builds and runs the Discord gateway client.
pub async fn run(config: Config) -> BotResult {
    let bot = Arc::new(Bot::new(config).await?);
    let token: Token = bot
        .config()
        .discord
        .bot_token
        .parse()
        .map_err(|error| BotError::Config(format!("invalid Discord bot token: {error}")))?;
    let http = HttpBuilder::new(token.clone()).build();
    let mut client = serenity::all::ClientBuilder::new_with_http(
        token,
        Arc::new(http),
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(bot.clone())
    .framework(Box::new(commands::framework(&bot)))
    .data(bot)
    .await?;
    client.start().await?;
    Ok(())
}

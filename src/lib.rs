#![recursion_limit = "256"]

//! Discord projection and supervision for ACP agents.

use std::{fmt, sync::Arc};

use serenity::all::{
    Context, EventHandler, FullEvent, GatewayIntents, GuildThread, HttpBuilder, PartialGuildThread,
    Token, async_trait,
};
use tracing::{debug, error, info, warn};

mod acp;
pub mod discord;
mod error;

pub mod config;
pub mod db;

pub use config::Config;
pub use db::Db;
pub use error::{BotError, BotResult, ModelSpecError};

/// Identifies whether a prompt already has a visible Discord message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptOrigin {
    /// The gateway message is already visible and must not be mirrored.
    AlreadyVisible,
    /// The prompt came from another surface and needs a webhook mirror.
    NeedsMirror,
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
    /// Discord context, webhook cache, and integration-owned runtime state.
    discord: discord::state::State,
    /// Per-thread ACP session actors.
    supervisor: acp::Supervisor,
}

impl Bot {
    /// Constructs application state using the configured state path.
    pub async fn new(config: Config) -> BotResult<Self> {
        info!(
            agents = config.agents.len(),
            "validating bot configuration..."
        );
        if let Err(error) = config.validate() {
            error!(?error, "bot configuration is invalid");
            return Err(error);
        }
        info!("bot configuration validated");
        let state_path = config::state_path();
        info!(path = ?state_path, "opening state database...");
        let db = match Db::open(&state_path).await {
            Ok(db) => db,
            Err(error) => {
                error!(?error, path = ?state_path, "failed to open state database");
                return Err(error);
            }
        };
        info!(path = ?state_path, "state database opened");
        Ok(Self::with_db(config, db))
    }

    /// Constructs application state with an already-open database.
    #[must_use]
    pub fn with_db(config: Config, db: Db) -> Self {
        Self(Arc::new(BotState {
            config,
            db,
            discord: discord::state::State::default(),
            supervisor: acp::Supervisor::default(),
        }))
    }

    /// Installs the Serenity context from the first ready event.
    pub fn install_context(&self, context: Context) {
        self.0.discord.install_context(context);
    }

    /// Returns the installed Serenity context.
    pub fn context(&self) -> BotResult<&Context> {
        self.0.discord.context()
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
            .ok_or(BotError::NotSession { thread })?;
        debug!(thread = ?thread, ?origin, "preparing acp prompt...");
        if origin == PromptOrigin::NeedsMirror
            && let Err(error) = self.mirror_user_message(thread, &text).await
        {
            warn!(?error, thread = ?thread, "failed to mirror prompt; forwarding it anyway...");
        }
        self.state().supervisor.prompt(self, &row, text, turn_id)
    }

    /// Changes the selected model for a session thread.
    pub async fn set_model(
        &self,
        thread: serenity::all::GenericChannelId,
        model: acp::ModelSpec,
    ) -> BotResult {
        let row = self
            .db()
            .session(thread)
            .await?
            .ok_or(BotError::NotSession { thread })?;
        self.state().supervisor.set_model(self, &row, model).await?;
        let current_model = self
            .session_ui(thread)
            .and_then(|ui| acp::default_model(&ui.config_options));
        if let Err(error) = self
            .update_session_starter(&row, current_model.as_deref())
            .await
        {
            warn!(?error, thread = ?thread, "failed to update session starter after model selection");
        }
        Ok(())
    }

    /// Deletes and re-imports the ACP session bound to one Discord thread.
    pub async fn reload_session(
        &self,
        thread: serenity::all::GenericChannelId,
    ) -> BotResult<serenity::all::GenericChannelId> {
        let row = self
            .db()
            .session(thread)
            .await?
            .ok_or(BotError::NotSession { thread })?;
        self.state().supervisor.stop_and_wait(self, thread).await?;
        let context = self.context()?.clone();
        info!(thread = ?thread, "deleting session thread...");
        thread.delete(&context.http, None).await?;
        info!(thread = ?thread, "deleted session thread");
        self.db().delete_session(thread).await?;
        self.import_session(&row.agent_key, &row.session_id).await
    }

    /// Returns cached ACP configuration options for an active session.
    #[must_use]
    pub(crate) fn session_ui(
        &self,
        thread: serenity::all::GenericChannelId,
    ) -> Option<acp::SessionUiState> {
        self.state().supervisor.session_ui(thread)
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
            .field("discord_ready", &self.0.discord.is_ready())
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
            .field("discord_ready", &self.discord.is_ready())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl EventHandler for Bot {
    /// Installs the Discord context and dispatches readiness and message
    /// events.
    async fn dispatch(&self, context: &Context, event: &FullEvent) {
        self.0.discord.install_context(context.clone());
        match event {
            FullEvent::Ready { data_about_bot, .. } => {
                info!(
                    application = %data_about_bot.application.id,
                    "discord ready; context installed"
                );
                info!("starting forum reconciliation...");
                match self.validate_and_reconcile_forum().await {
                    Ok(()) => info!("forum reconciliation completed"),
                    Err(error) => {
                        warn!(?error, "configured forum is unavailable");
                    }
                }
                info!("starting prompt webhook reconciliation...");
                match self.validate_and_reconcile_webhook().await {
                    Ok(()) => info!("prompt webhook reconciliation completed"),
                    Err(error) => {
                        warn!(?error, "prompt webhook could not be initialized");
                    }
                }
            }
            FullEvent::Message { new_message, .. } => {
                if let Err(error) = self.handle_message(new_message).await {
                    warn!(?error, "failed to handle discord message");
                }
            }
            FullEvent::ThreadCreate { thread, .. } => {
                if let Err(error) = self.handle_thread_create(thread).await {
                    warn!(?error, thread = ?thread.id, "failed to remove unmanaged forum thread");
                }
            }
            FullEvent::ThreadDelete { thread, .. } => {
                if let Err(error) = self.handle_thread_delete(thread).await {
                    warn!(?error, thread = ?thread.id, "failed to remove deleted session");
                }
            }
            _ => {}
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
        info!(
            thread = ?message.channel_id,
            message = %message.id,
            characters = message.content.chars().count(),
            "received prompt"
        );
        if let Err(error) = self
            .forward_prompt(
                message.channel_id,
                message.content.to_string(),
                message.id.to_string(),
                PromptOrigin::AlreadyVisible,
            )
            .await
        {
            warn!(?error, thread = ?message.channel_id, "acp `session/prompt` failed");
            if let Err(reply_error) = message
                .reply(
                    &context.http,
                    format!("acp `session/prompt` failed: {error}"),
                )
                .await
            {
                warn!(?reply_error, thread = ?message.channel_id, "failed to report acp `session/prompt` failure");
            } else {
                debug!(thread = ?message.channel_id, "reported acp prompt failure");
            }
        } else {
            info!(thread = ?message.channel_id, message = %message.id, "queued prompt");
        }
        Ok(())
    }

    /// Removes a newly created forum thread unless the bot owns it.
    async fn handle_thread_create(&self, thread: &GuildThread) -> BotResult {
        if thread.parent_id != self.config().discord.forum_channel_id {
            return Ok(());
        }
        if thread.owner_id == self.context()?.cache.current_user().id {
            debug!(thread = ?thread.id, "observed bot-created forum thread");
            return Ok(());
        }
        info!(thread = ?thread.id, "deleting unmanaged forum thread...");
        let context = self.context()?.clone();
        thread.id.widen().delete(&context.http, None).await?;
        info!(thread = ?thread.id, "deleted unmanaged forum thread");
        Ok(())
    }

    /// Removes the local binding when Discord deletes a managed thread.
    async fn handle_thread_delete(&self, thread: &PartialGuildThread) -> BotResult {
        if thread.parent_id != self.config().discord.forum_channel_id {
            return Ok(());
        }
        let thread_id = thread.id.widen();
        debug!(thread = ?thread_id, "observed forum thread deletion");
        if self.db().session(thread_id).await?.is_none() {
            return Ok(());
        }
        info!(thread = ?thread_id, "stopping deleted managed session...");
        self.state()
            .supervisor
            .stop_and_wait(self, thread_id)
            .await?;
        self.db().delete_session(thread_id).await?;
        info!(thread = ?thread_id, "removed deleted managed session");
        Ok(())
    }
}

/// Builds and runs the Discord gateway client.
pub async fn run(config: Config) -> BotResult {
    info!("initializing bot state...");
    let bot = Arc::new(Bot::new(config).await?);
    info!("bot state initialized");
    let token: Token = match bot.config().discord.bot_token.parse::<Token>() {
        Ok(token) => token,
        Err(error) => {
            error!(?error, "failed to parse discord token");
            return Err(BotError::InvalidDiscordToken {
                message: error.to_string(),
            });
        }
    };
    info!("discord token parsed");
    let http = HttpBuilder::new(token.clone()).build();
    info!("discord http client built");
    info!("building discord client...");
    let mut client = match serenity::all::ClientBuilder::new_with_http(
        token,
        Arc::new(http),
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(bot.clone())
    .framework(Box::new(discord::commands::framework(&bot)))
    .data(bot)
    .await
    {
        Ok(client) => client,
        Err(error) => {
            error!(?error, "failed to build discord client");
            return Err(error.into());
        }
    };
    info!("discord client built; starting gateway...");
    if let Err(error) = client.start().await {
        error!(?error, "discord gateway stopped with an error");
        return Err(error.into());
    }
    info!("discord gateway stopped");
    Ok(())
}

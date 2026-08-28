//! Discord projection and supervision for arbitrary Agent Client Protocol
//! agents.

mod acp;
mod commands;
pub mod config;
mod db;
mod elicitation;
mod error;
mod forum;
mod permission;
mod projects;
mod render;
mod webhook;

use std::{
    collections::HashMap,
    fmt::{self, Debug, Formatter},
    ops::Deref,
    sync::{Arc, Mutex, OnceLock},
};

pub use config::Config;
use db::Db;
pub use error::BotError;
use projects::Project;
use serenity::all::{
    ClientBuilder, Context, EventHandler, FullEvent, GatewayIntents, HttpBuilder, Token, UserId,
    Webhook, async_trait,
};
use tracing::{info, warn};

/// Result type used by Agentcord operations.
pub type BotResult<T = ()> = Result<T, BotError>;

/// Cheaply cloneable handle to the shared application state.
#[derive(Clone)]
pub struct Bot(
    /// Single shared ownership boundary for application state.
    Arc<BotState>,
);

/// Shared application dependencies and runtime coordination state.
#[doc(hidden)]
pub struct BotState {
    /// Immutable validated configuration.
    pub(crate) config: Config,
    /// Persistent session and render mappings.
    pub(crate) db: Db,
    /// Discord context installed by the first ready event.
    context: OnceLock<Context>,
    /// Singleflight registry for starting and active ACP sessions.
    pub(crate) sessions: acp::SessionRegistry,
    /// Short-lived per-agent session-list cache.
    pub(crate) listings: Mutex<HashMap<String, acp::CachedListing>>,
    /// Learned `session/load` support keyed by agent.
    pub(crate) restorable: Mutex<HashMap<String, bool>>,
    /// Recoverable cached webhook used to mirror user prompts.
    pub(crate) webhook: tokio::sync::Mutex<Option<Webhook>>,
    /// Retryable one-time startup and restoration gate.
    ready_started: tokio::sync::OnceCell<()>,
}

impl Debug for BotState {
    /// Formats only stable, useful state and omits runtime handles.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BotState")
            .field("config", &self.config)
            .field("db", &self.db)
            .field("sessions", &self.sessions)
            .finish_non_exhaustive()
    }
}

impl Deref for Bot {
    /// Shared state exposed through the lightweight bot handle.
    type Target = BotState;

    /// Exposes the shared application state to crate modules.
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Debug for Bot {
    /// Formats the bot without leaking tokens or noisy runtime state.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bot")
            .field("config", &self.0.config)
            .finish_non_exhaustive()
    }
}

impl Bot {
    /// Validates configuration and constructs the shared application state.
    fn new(config: Config) -> BotResult<Self> {
        config.validate()?;
        let db = Db::open(&config::state_path())?;
        Ok(Self(Arc::new(BotState {
            config,
            db,
            context: OnceLock::new(),
            sessions: acp::SessionRegistry::default(),
            listings: Mutex::default(),
            restorable: Mutex::default(),
            webhook: tokio::sync::Mutex::new(None),
            ready_started: tokio::sync::OnceCell::new(),
        })))
    }

    /// Returns the Discord context after the first ready event.
    pub(crate) fn context(&self) -> BotResult<&Context> {
        self.context
            .get()
            .ok_or_else(|| BotError::Other("Discord is not ready".into()))
    }

    /// Reports whether a Discord user may interact with the bot.
    #[must_use]
    pub fn is_allowed(&self, user: UserId) -> bool {
        user == self.config.discord.allowed_user_id
    }

    /// Resolves user-supplied project input against configured project roots.
    pub(crate) fn resolve_project(&self, input: &str) -> BotResult<Project> {
        projects::resolve(&self.config.projects, input)
    }

    /// Performs one-time forum reconciliation and session restoration.
    ///
    /// Concurrent ready events share the same initialization attempt, while a
    /// failed attempt remains retryable on a later gateway reconnect.
    async fn handle_ready(&self, ctx: &Context) {
        let _ = self.context.set(ctx.clone());
        let result = self
            .ready_started
            .get_or_try_init(|| async {
                self.validate_and_reconcile_forum().await?;
                self.restore_all().await;
                Ok::<(), BotError>(())
            })
            .await;
        if let Err(error) = result {
            warn!(?error, "configured Agentcord forum is unavailable");
            return;
        }
        info!("agentcord ready");
    }

    /// Routes an allowed Discord message to its thread's ACP session.
    async fn handle_message(&self, message: &serenity::all::Message) {
        let Ok(ctx) = self.context() else {
            return;
        };
        if !self.is_allowed(message.author.id)
            || message.author.id == ctx.cache.current_user().id
            || message.content.trim().is_empty()
        {
            return;
        }
        let Ok(Some(_)) = self.db.session(message.channel_id) else {
            return;
        };
        if let Err(error) = self
            .submit(message.channel_id, message.content.to_string())
            .await
        {
            let _ = message
                .reply(&ctx.http, format!("couldn't send to ACP: {error}"))
                .await;
        }
    }
}

#[async_trait]
impl EventHandler for Bot {
    /// Projects relevant Discord gateway events into application operations.
    async fn dispatch(&self, ctx: &Context, event: &FullEvent) {
        match event {
            FullEvent::Ready { .. } => self.handle_ready(ctx).await,
            FullEvent::Message { new_message, .. } => self.handle_message(new_message).await,
            FullEvent::ThreadCreate { thread, .. } => {
                if let Err(error) = self.delete_manual_post(thread).await {
                    warn!(?error, thread = %thread.base.name, "failed to handle forum post");
                }
            }
            FullEvent::ThreadDelete { thread, .. } => {
                let channel = thread.id.widen();
                if self.db.session(channel).ok().flatten().is_some() {
                    self.forget(channel);
                    if let Err(error) = self.db.delete_session(channel) {
                        warn!(?error, ?channel, "failed to delete removed session binding");
                    }
                }
            }
            _ => {}
        }
    }
}

/// Builds and runs the Discord client until it shuts down or fails.
pub async fn run(config: Config) -> BotResult {
    let bot = Arc::new(Bot::new(config)?);
    let token: Token = bot
        .config
        .discord
        .bot_token
        .parse()
        .map_err(|error| BotError::Config(format!("invalid Discord bot token: {error}")))?;
    let http = HttpBuilder::new(token.clone()).build();
    let mut client = ClientBuilder::new_with_http(
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

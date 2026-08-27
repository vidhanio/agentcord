mod acp;
mod commands;
pub mod config;
mod db;
mod error;
mod forum;
mod permission;
mod projects;
mod render;

use std::{
    collections::HashMap,
    fmt::{self, Debug, Formatter},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub use config::Config;
use db::Db;
pub use error::BotError;
use projects::ProjectCatalog;
use serenity::all::{
    ClientBuilder, Context, EventHandler, FullEvent, GatewayIntents, GenericChannelId, HttpBuilder,
    Token, UserId, async_trait,
};
use tracing::{info, warn};

pub type BotResult<T = ()> = Result<T, BotError>;

#[derive(Clone)]
pub struct Bot {
    pub(crate) config: Arc<Config>,
    pub(crate) db: Db,
    pub(crate) projects: Arc<ProjectCatalog>,
    context: Arc<OnceLock<Context>>,
    pub(crate) active: Arc<Mutex<HashMap<GenericChannelId, acp::ActiveSession>>>,
    pub(crate) render_locks: Arc<Mutex<HashMap<GenericChannelId, Arc<tokio::sync::Mutex<()>>>>>,
    pub(crate) resume_locks: Arc<Mutex<HashMap<GenericChannelId, Arc<tokio::sync::Mutex<()>>>>>,
    pub(crate) next_generation: Arc<AtomicU64>,
    ready_started: Arc<AtomicBool>,
}

impl Debug for Bot {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bot")
            .field("config", &self.config)
            .field("projects", &self.projects)
            .finish_non_exhaustive()
    }
}

impl Bot {
    fn new(config: Config) -> BotResult<Self> {
        config.validate()?;
        let projects = ProjectCatalog::discover(&config.projects)?;
        let db = Db::open(&config::state_path())?;
        Ok(Self {
            config: Arc::new(config),
            db,
            projects: Arc::new(projects),
            context: Arc::default(),
            active: Arc::default(),
            render_locks: Arc::default(),
            resume_locks: Arc::default(),
            next_generation: Arc::new(AtomicU64::new(1)),
            ready_started: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn context(&self) -> BotResult<&Context> {
        self.context
            .get()
            .ok_or_else(|| BotError::Other("Discord is not ready".into()))
    }

    #[must_use]
    pub fn is_allowed(&self, user: UserId) -> bool {
        user == self.config.discord.allowed_user_id
    }

    async fn handle_ready(&self, ctx: &Context) {
        let _ = self.context.set(ctx.clone());
        if self.ready_started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.validate_and_reconcile_forum().await {
            self.ready_started.store(false, Ordering::Release);
            warn!(?error, "configured Agentcord forum is unavailable");
            return;
        }
        self.restore_all().await;
        info!("agentcord ready");
    }

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

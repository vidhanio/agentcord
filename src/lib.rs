mod commands;
pub mod config;
pub mod db;
mod error;
mod forum;
pub mod herdr;
mod relay;
mod session;
mod utils;

use std::{
    fmt::{self, Debug, Formatter},
    path::PathBuf,
    sync::Arc,
};

pub use db::Db;
use error::BotError;
use forum::Forum;
use herdr::{Herdr, SessionPath};
use relay::{Relay, RelayJob};
use serenity::all::{
    ChannelId, ClientBuilder, Context, EventHandler, FullEvent, GatewayIntents, HttpBuilder,
    Message, Token, UserId, async_trait,
};
pub use session::{Harness, SessionRole, read_session, read_session_messages, read_session_title};
use tracing::{info, warn};

pub use self::config::Config;

pub type BotResult<T = ()> = Result<T, BotError>;

/// The Discord bot: relays forum posts to herdr agents.
#[derive(Clone)]
pub struct Bot {
    pub(crate) config: Arc<Config>,
    pub(crate) herdr: Herdr,
    pub(crate) db: Db,
    pub(crate) forum: Arc<Forum>,
    pub(crate) relay: Arc<Relay>,
}

impl Debug for Bot {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bot")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Bot {
    async fn new(config: Config) -> BotResult<Self> {
        let config = Arc::new(config);

        // Open the state database (workspaces and session bindings) before
        // anything else so configuration problems surface early.
        let db_path = state_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                BotError::Other(format!(
                    "failed to create state directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let db = Db::open(&db_path)
            .await
            .map_err(|error| BotError::Other(format!("failed to open state database: {error}")))?;

        let herdr = Herdr::new(
            crate::config::socket_path(),
            crate::config::OPERATION_TIMEOUT,
        );

        let forum = Arc::new(Forum::new(config.clone(), herdr.clone(), db.clone()));
        let relay = Arc::new(Relay::new(herdr.clone(), forum.clone()));

        Ok(Self {
            config,
            herdr,
            db,
            forum,
            relay,
        })
    }

    async fn event_loop(bot: Arc<Self>, ctx: Context) {
        // Message mirroring runs on its own task: a stuck Discord call in a
        // sync must never stall herdr event handling. The poll runs on a
        // fixed tick, so the startup reconcile (capped catch-up) always
        // runs first.
        tokio::spawn({
            let forum = bot.forum.clone();
            let ctx = ctx.clone();
            async move { forum.poll_loop(ctx).await }
        });
        bot.forum.run_event_loop(ctx).await;
    }

    /// Whether `user_id` may run commands and talk to agents: everyone when
    /// no allowed user is configured, otherwise only that user.
    #[must_use]
    pub(crate) fn is_allowed(&self, user_id: UserId) -> bool {
        self.config
            .allowed_user_id
            .is_none_or(|allowed| allowed == user_id)
    }

    /// Resumes a dead session's agent with `message` as the first prompt,
    /// or replies when the session is already starting or the resume fails.
    async fn resume_session_and_relay(
        &self,
        ctx: &Context,
        message: &Message,
        session: &db::SessionRow,
    ) {
        match self.forum.resume_session(ctx, session).await {
            Ok(Some(started)) => {
                let session_path = started.agent_session.as_ref().map_or_else(
                    || SessionPath::from(session.session_path.clone()),
                    |agent_session| agent_session.value.clone(),
                );
                if let Err(error) = self
                    .relay
                    .submit(
                        ctx.clone(),
                        &started.pane_id,
                        RelayJob {
                            channel_id: ChannelId::new(message.channel_id.get()),
                            session_path,
                            text: message.content.clone().into(),
                        },
                    )
                    .await
                {
                    warn!(?error, "failed to queue resume relay job");
                }
            }
            Ok(None) => {
                if let Err(error) = message
                    .reply(
                        &ctx.http,
                        "this session is starting up — send your message again in a moment.",
                    )
                    .await
                {
                    warn!(?error, "failed to reply about session startup");
                }
            }
            Err(error) => {
                warn!(?error, session = %session.session_path, "failed to resume session");
                if let Err(reply_error) = message
                    .reply(&ctx.http, format!("failed to resume this session: {error}"))
                    .await
                {
                    warn!(?reply_error, "failed to reply about session resume failure");
                }
            }
        }
    }
}

pub async fn run(config: Config) -> BotResult {
    let bot = Bot::new(config).await?;

    info!("building client...");

    // The default ratelimiter and request pipeline: no custom client, no
    // disabled ratelimiting — the next-branch ratelimiter no longer holds
    // locks across requests, so the wedging the old one caused is gone.
    let token: Token = bot
        .config
        .discord_bot_token
        .parse()
        .map_err(|error| BotError::Other(format!("invalid Discord bot token: {error}")))?;
    let http = HttpBuilder::new(token.clone()).build();

    let bot = Arc::new(bot);
    let mut client = ClientBuilder::new_with_http(
        token,
        Arc::new(http),
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(bot.clone())
    .framework(Box::new(commands::framework(&bot)))
    .data(bot.clone())
    .await?;

    info!("starting client...");

    client.start().await?;

    Ok(())
}

#[async_trait]
impl EventHandler for Bot {
    async fn dispatch(&self, ctx: &Context, event: &FullEvent) {
        match event {
            FullEvent::Ready { .. } => {
                info!("bot ready");
                tokio::spawn(Self::event_loop(Arc::new(self.clone()), ctx.clone()));
            }
            FullEvent::ThreadCreate { thread, .. } => {
                if let Err(error) = self.forum.handle_thread_create(ctx, thread).await {
                    warn!(?error, thread = %thread.base.name, "failed to handle new forum post");
                }
            }
            FullEvent::Message { new_message, .. } => self.handle_message(ctx, new_message).await,
            // Interactions (the `/agent` and `/herdr` commands, the agent
            // modal) are handled by the poise framework.
            _ => {}
        }
    }
}

impl Bot {
    async fn handle_message(&self, ctx: &Context, message: &Message) {
        if !self.is_allowed(message.author.id) {
            return;
        }

        if message.author.id == ctx.cache.current_user().id {
            return;
        }

        if message.content.trim().is_empty() {
            return;
        }

        // Only messages in managed forum posts are relayed.
        let Ok(post_id) = i64::try_from(message.channel_id.get()) else {
            return;
        };
        let session = match self.db.session_by_post(post_id).await {
            Ok(Some(session)) => session,
            Ok(None) => return, // Unmanaged channel.
            Err(error) => {
                warn!(?error, "failed to look up session for post");
                return;
            }
        };

        // Find the live agent bound to this session.
        let agents = match self.herdr.list_agents().await {
            Ok(agents) => agents,
            Err(error) => {
                warn!(?error, "failed to list agents for message relay");
                return;
            }
        };

        let Some(agent) = agents.iter().find(|agent| {
            agent
                .agent_session
                .as_ref()
                .is_some_and(|session_ref| session.hosts(session_ref.value.as_str()))
        }) else {
            // No live agent hosts this session: re-launch it in place,
            // resuming the same conversation, and relay the message to the
            // new agent.
            self.resume_session_and_relay(ctx, message, &session).await;
            return;
        };

        // Agents are addressed by their pane id, which herdr accepts
        // anywhere it takes an agent name.
        let target = &agent.pane_id;

        if let Err(error) = self
            .relay
            .submit(
                ctx.clone(),
                target,
                RelayJob {
                    channel_id: ChannelId::new(message.channel_id.get()),
                    session_path: SessionPath::from(session.session_path.clone()),
                    text: message.content.clone().into(),
                },
            )
            .await
        {
            warn!(?error, "failed to queue relay job");
        }
    }
}

/// Resolves the state database path: `<state dir>/herdcord.sqlite`.
#[must_use]
fn state_db_path() -> PathBuf {
    crate::config::state_dir().join("herdcord.sqlite")
}

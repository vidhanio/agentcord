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
use forum::{Forum, LaunchSpec};
use herdr::{Herdr, SessionPath};
use relay::{Relay, RelayJob};
use serenity::all::{
    Channel, ClientBuilder, Context, CreateMessage, EventHandler, GatewayIntents, GuildChannel,
    Message, Ready, UserId, async_trait,
};
pub use session::{
    AgentKind, SessionRole, read_session, read_session_messages, read_session_title,
};
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
    fn is_allowed(&self, user_id: UserId) -> bool {
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
                            channel_id: message.channel_id,
                            session_path,
                            text: message.content.clone(),
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
                        ctx,
                        "This session is starting up — send your message again in a moment.",
                    )
                    .await
                {
                    warn!(?error, "failed to reply about session startup");
                }
            }
            Err(error) => {
                warn!(?error, session = %session.session_path, "failed to resume session");
                if let Err(reply_error) = message
                    .reply(ctx, format!("Failed to resume this session: {error}"))
                    .await
                {
                    warn!(?reply_error, "failed to reply about session resume failure");
                }
            }
        }
    }

    /// Spawns the agent for a host-created forum post, binds its session to
    /// a post, relays the post's prompt to it, and DMs the host the new
    /// session thread.
    async fn launch_from_post(&self, ctx: &Context, spec: &LaunchSpec) -> BotResult<()> {
        let started = match &spec.workspace {
            Some(workspace) => {
                self.forum
                    .spawn_in_workspace(workspace, &spec.name, spec.kind, &spec.cwd, &[])
                    .await?
            }
            None => {
                self.forum
                    .spawn_in_new_workspace(&spec.name, &spec.name, spec.kind, &spec.cwd, &[])
                    .await?
            }
        };

        if let Err(error) = self.forum.ensure_session_post(ctx, &started).await {
            warn!(?error, name = %spec.name, "failed to bind launched session to a post");
        }

        // The host's post was deleted; DM them the new session thread so
        // the conversation has a home. The link is the whole message.
        if let Some(session_path) = started
            .agent_session
            .as_ref()
            .map(|session| session.value.clone())
            && let Ok(Some(session)) = self.db.get_session(&session_path).await
            && let Some(post_id) = session.post_channel_id
        {
            let link = format!(
                "https://discord.com/channels/{}/{}",
                self.config.guild_id, post_id
            );
            if let Err(error) = spec
                .author
                .dm(ctx, CreateMessage::new().content(link))
                .await
            {
                warn!(?error, name = %spec.name, "failed to DM post author about the session thread");
            }
        }

        if spec.prompt.is_empty() {
            return Ok(());
        }
        // The agent's session reference may lag the launch; the empty path
        // makes the post-prompt sync a no-op until the poll picks the
        // session up.
        let session_path = started.agent_session.as_ref().map_or_else(
            || SessionPath::from(String::new()),
            |session| session.value.clone(),
        );
        self.relay
            .submit(
                ctx.clone(),
                &started.pane_id,
                RelayJob {
                    channel_id: spec.post,
                    session_path,
                    text: spec.prompt.clone(),
                },
            )
            .await?;
        Ok(())
    }
}

pub async fn run(config: Config) -> BotResult {
    let bot = Bot::new(config).await?;

    info!("building client...");

    // Serenity's ratelimiter has wedged twice in production — a request
    // freezing inside its queue holds every Discord write hostage, with no
    // timeout anywhere. Run without it (the bot's volume is far below
    // Discord's limits) and bound each request with a timeout so nothing
    // can hang the bot indefinitely.
    let http = serenity::all::HttpBuilder::new(&bot.config.discord_bot_token)
        .client(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| {
                    BotError::Other(format!("failed to build the Discord HTTP client: {error}"))
                })?,
        )
        .ratelimiter_disabled(true)
        .build();

    let mut client = ClientBuilder::new_with_http(
        http,
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(bot)
    .await?;

    info!("starting client...");

    client.start().await?;

    Ok(())
}

#[async_trait]
impl EventHandler for Bot {
    async fn ready(&self, ctx: Context, _: Ready) {
        info!("bot ready");
        tokio::spawn(Self::event_loop(Arc::new(self.clone()), ctx));
    }

    async fn thread_create(&self, ctx: Context, thread: GuildChannel) {
        let launch = match self.forum.handle_thread_create(&ctx, &thread).await {
            Ok(launch) => launch,
            Err(error) => {
                warn!(?error, thread = %thread.name, "failed to handle new forum post");
                return;
            }
        };
        let Some(spec) = launch else {
            return;
        };

        let outcome = self.launch_from_post(&ctx, &spec).await;

        // The host's post never survives, launch or not.
        if let Err(error) = spec.post.delete(&ctx.http).await {
            warn!(?error, thread = %thread.name, "failed to delete host post after launch");
        }

        if let Err(error) = outcome
            && let Err(dm_error) = spec
                .author
                .dm(
                    &ctx,
                    CreateMessage::new().content(format!(
                        "I couldn't launch an agent from your post: {error}"
                    )),
                )
                .await
        {
            warn!(?dm_error, "failed to DM post author about launch failure");
        }
    }

    async fn message(&self, ctx: Context, new_message: Message) {
        let message = &new_message;

        if !self.is_allowed(message.author.id) {
            return;
        }

        if message.author.id == ctx.cache.current_user().id {
            return;
        }

        if message.content.trim().is_empty() {
            return;
        }

        let channel = match ctx
            .cache
            .guild(self.config.guild_id)
            .and_then(|guild| guild.channels.get(&message.channel_id).cloned())
        {
            Some(channel) => channel,
            None => match message.channel_id.to_channel(&ctx).await {
                Ok(Channel::Guild(channel)) => channel,
                _ => return,
            },
        };

        // Only messages in managed forum posts are relayed.
        let Ok(post_id) = i64::try_from(channel.id.get()) else {
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
            self.resume_session_and_relay(&ctx, message, &session).await;
            return;
        };

        // Agents are addressed by their pane id, which herdr accepts
        // anywhere it takes an agent name.
        let target = &agent.pane_id;

        if let Err(error) = self
            .relay
            .submit(
                ctx,
                target,
                RelayJob {
                    channel_id: message.channel_id,
                    session_path: SessionPath::from(session.session_path.clone()),
                    text: message.content.clone(),
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

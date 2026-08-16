//! Forum-side state: per-workspace forum channels and session-bound posts.
//!
//! The [`Forum`] struct and its shared state live here; the behaviour is
//! split across focused submodules:
//!
//! - [`tags`] — forum tag management (status + harness tags, post metadata)
//! - [`workspace`] — the workspace↔forum lifecycle (ensure/rename forums,
//!   worktree resolution, workspace lookups, stale-row pruning)
//! - [`post`] — the session↔post binding lifecycle (ensure/create posts, thread
//!   handling, session lookups)
//! - [`spawn`] — agent spawn helpers, naming, and launch cwd
//! - [`resume`] — re-launching dead sessions in place
//! - [`sync`] — the transcript mirror (cursor syncs, tool embeds)
//! - [`echo`] — webhook echoes of the user's herdr-typed turns
//! - [`render`] — rendering tool calls and long text into Discord messages
//! - [`events`] — the herdr event loop, the reconcile drift backstop, and pane
//!   lifecycle handling
//! - [`poll`] — the fixed-tick transcript poll and rotation adoption
//! - [`titles`] — post titles and the starter message
//! - [`lookup`] — resolving Discord channels for the bot's bindings

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use serenity::all::ChannelId;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    BotResult,
    config::Config,
    db::Db,
    error::BotError,
    herdr::{Herdr, PaneId, SessionPath},
    session::{ToolCallId, ToolState},
};

mod echo;
mod events;
mod lookup;
mod poll;
mod post;
mod render;
mod resume;
mod spawn;
mod sync;
mod tags;
mod titles;
mod workspace;

/// Posted tool-embed bookkeeping: session path + tool call id → the posted
/// message id and the state it currently shows.
type ToolMessages =
    Arc<Mutex<HashMap<(SessionPath, ToolCallId), (serenity::all::MessageId, ToolState)>>>;

/// The transcript stamp (mtime, size) each live session's bound file had
/// at the poll's last parse, so an unchanged file costs one stat instead of
/// a full mirror pass. Keyed by session path — a rotation adoption opens
/// the gate naturally, because the new file has a fresh stamp.
type TranscriptStamps = Arc<Mutex<HashMap<SessionPath, (std::time::SystemTime, u64)>>>;

/// Forum-side state: workspace forums, session-bound posts, and transcript
/// syncing.
#[derive(Debug, Clone)]
pub struct Forum {
    config: Arc<Config>,
    herdr: Herdr,
    db: Db,
    /// pane id → session path, the one piece of in-memory state: it lets a
    /// `pane.closed` event mark the session's post dead instantly, and it
    /// is the poll's live-session set. Everything else is queried live from
    /// herdr or Discord. Entries are removed when a pane closes or
    /// reconcile finds the pane gone.
    sessions_by_pane: Arc<Mutex<HashMap<PaneId, SessionPath>>>,
    /// Sessions currently being resumed, so two messages in a dead thread
    /// cannot launch two agents.
    resuming: Arc<Mutex<HashSet<SessionPath>>>,
    /// Only touched under `sync_lock`; entries are dropped when their
    /// session dies.
    tool_messages: ToolMessages,
    /// The poll's mirror gate; entries are dropped when their session dies.
    transcript_stamps: TranscriptStamps,
    /// Serializes transcript syncs: the poll and the relay settle can fire
    /// concurrently, and two syncs reading the same cursor would post
    /// duplicate messages.
    sync_lock: Arc<AsyncMutex<()>>,
}
impl Forum {
    #[must_use]
    pub fn new(config: Arc<Config>, herdr: Herdr, db: Db) -> Self {
        Self {
            config,
            herdr,
            db,
            sessions_by_pane: Arc::new(Mutex::new(HashMap::new())),
            resuming: Arc::new(Mutex::new(HashSet::new())),
            tool_messages: Arc::new(Mutex::new(HashMap::new())),
            transcript_stamps: Arc::new(Mutex::new(HashMap::new())),
            sync_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Drops the in-memory bookkeeping of a dead session: its posted tool
    /// embeds and its transcript mirror stamp.
    fn drop_session_bookkeeping(&self, session_path: &str) {
        self.tool_messages
            .lock()
            .expect("tool_messages lock poisoned")
            .retain(|(path, _), _| path.as_str() != session_path);
        self.transcript_stamps
            .lock()
            .expect("transcript_stamps lock poisoned")
            .retain(|path, _| path.as_str() != session_path);
    }
}

/// Converts a Discord snowflake (channel or message id) to the database's
/// i64 representation.
pub fn to_i64(id: impl Into<u64>) -> BotResult<i64> {
    i64::try_from(id.into()).map_err(|_| BotError::Other("snowflake overflows i64".into()))
}

/// Converts a database i64 back into a Discord channel id.
pub fn from_i64(id: i64) -> BotResult<ChannelId> {
    u64::try_from(id)
        .map(ChannelId::new)
        .map_err(|_| BotError::Other(format!("{id} is not a valid channel id")))
}

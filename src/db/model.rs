//! The persisted row types: a workspace and its forum channel, and a
//! session bound to a forum post.

/// A herdr workspace and its persistent forum channel.
#[derive(Debug, Clone, toasty::Model)]
pub(crate) struct WorkspaceRow {
    /// The workspace label — the stable identity the bot keys on.
    #[key]
    pub label: String,

    /// herdr's positional workspace id (e.g. `"w3"`), kept so a rename can
    /// re-key the row to the new label; empty for rows that predate id
    /// tracking until the first reconcile re-keys them.
    pub workspace_id: String,

    /// Discord snowflake of the forum channel, if configured.
    pub forum_channel_id: Option<i64>,
}

/// A session (agent launch) bound to a forum post.
///
/// Stores only what neither herdr nor Discord can tell us: the session↔post
/// binding and the transcript sync cursors. Live session state lives in
/// herdr; posted messages live in Discord.
#[derive(Debug, Clone, toasty::Model)]
pub(crate) struct SessionRow {
    /// `agent_session.value`, unique per launch.
    #[key]
    pub session_path: String,

    /// Forum post thread id, if the session is attached to one.
    pub post_channel_id: Option<i64>,

    /// The label of the herdr workspace the agent ran in, for instant post
    /// inactivation on `workspace.closed` and resume spawn.
    pub workspace_label: String,

    /// Working directory of the agent's pane, for the starter message once
    /// the agent is gone.
    pub cwd: String,

    /// The transcript file synced into the post. Starts equal to
    /// `session_path`; when omp rotates the transcript of a session
    /// replaced in the same pane (and herdr keeps reporting the old path),
    /// the poll re-binds this to the new file.
    pub transcript_path: String,

    /// Conversation messages already posted to Discord.
    pub synced_messages: i64,

    /// Discord id of the last message posted for this session.
    pub last_discord_message_id: Option<i64>,

    /// Discord id of the post's starter message (its first message, the
    /// session intro), captured at post creation so the intro can be
    /// refreshed as post metadata.
    pub starter_message_id: Option<i64>,
}

impl SessionRow {
    /// Whether a live agent's reported session value belongs to this row:
    /// the row's own key, or the transcript it adopted after a rotation.
    #[must_use]
    pub fn hosts(&self, session_value: &str) -> bool {
        session_value == self.session_path || session_value == self.transcript_path
    }
}

//! SQLite persistence for herdcord: workspaces and the sessions launched
//! inside them.
//!
//! Backed by [toasty] 0.10 with the `sqlite` driver. The [`Db`] wrapper is
//! the module's public API; `WorkspaceRow` and `SessionRow` are the flat
//! row types it persists. The row fields mirror the herdr wire format, so
//! they stay plain strings rather than enums.
//!
//! [toasty]: https://docs.rs/toasty

use std::path::Path;

use crate::herdr::SessionPath;

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

/// The toasty schema push is not idempotent: this checks whether the session
/// table already exists so reopening an existing database skips it.
async fn tables_exist(db: &toasty::Db) -> toasty::Result<bool> {
    let mut conn = db.connection().await?;
    let rows = toasty::sql::query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_rows' LIMIT 1",
    )
    .exec(&mut conn)
    .await?;
    Ok(!rows.is_empty())
}

/// Migrates databases created before the workspace identity became the
/// label: `workspace_rows.workspace_id` becomes the `label` key (with a new
/// `workspace_id` column added), and `session_rows.workspace_id` becomes
/// `workspace_label`. Each statement is a no-op on schemas that already
/// have the new shape, so the SQLite errors they raise are expected.
async fn migrate_legacy_columns(db: &toasty::Db) -> toasty::Result<()> {
    let mut conn = db.connection().await?;
    for statement in [
        "ALTER TABLE workspace_rows RENAME COLUMN workspace_id TO label",
        "ALTER TABLE workspace_rows ADD COLUMN workspace_id TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE session_rows RENAME COLUMN workspace_id TO workspace_label",
    ] {
        if let Err(error) = toasty::sql::statement(statement).exec(&mut conn).await {
            tracing::debug!(?error, "legacy schema migration not applicable");
        }
    }
    Ok(())
}

/// SQLite-backed store for workspaces and sessions.
///
/// Cloning is cheap: the handle shares the underlying connection pool.
#[derive(Debug, Clone)]
pub struct Db {
    db: toasty::Db,
}

impl Db {
    /// Connects to the SQLite database at `url` and pushes the schema
    /// derived from the registered models when the tables are missing, so
    /// reopening an existing database is a no-op.
    async fn open_connected(url: &str) -> toasty::Result<Self> {
        let db = toasty::Db::builder()
            .models(toasty::models!(crate::*))
            .connect(url)
            .await?;
        if tables_exist(&db).await? {
            migrate_legacy_columns(&db).await?;
        } else {
            db.push_schema().await?;
        }
        Ok(Self { db })
    }

    /// Opens (creating on first use) the SQLite database at `path`.
    pub async fn open(path: &Path) -> toasty::Result<Self> {
        Self::open_connected(&format!("sqlite:{}", path.display())).await
    }

    /// Opens an ephemeral in-memory SQLite database and pushes the schema.
    ///
    /// Intended for tests; each call produces a fresh, empty database.
    pub async fn open_in_memory() -> toasty::Result<Self> {
        Self::open_connected("sqlite::memory:").await
    }

    /// Returns the workspace with the given label, if any.
    pub(crate) async fn get_workspace(&self, label: &str) -> toasty::Result<Option<WorkspaceRow>> {
        let mut conn = self.db.connection().await?;
        let row = WorkspaceRow::filter(WorkspaceRow::fields().label().eq(label))
            .first()
            .exec(&mut conn)
            .await?;
        Ok(row)
    }

    /// Returns the workspace with the given herdr id, if any — used to
    /// re-key the row when the workspace is renamed.
    pub(crate) async fn get_workspace_by_id(
        &self,
        workspace_id: &str,
    ) -> toasty::Result<Option<WorkspaceRow>> {
        let mut conn = self.db.connection().await?;
        let row = WorkspaceRow::filter(WorkspaceRow::fields().workspace_id().eq(workspace_id))
            .first()
            .exec(&mut conn)
            .await?;
        Ok(row)
    }

    /// Inserts `row`, or replaces the workspace with the same label.
    pub(crate) async fn upsert_workspace(&self, row: &WorkspaceRow) -> toasty::Result<()> {
        let mut conn = self.db.connection().await?;
        WorkspaceRow::upsert_by_label(&row.label)
            .workspace_id(row.workspace_id.clone())
            .forum_channel_id(row.forum_channel_id)
            .exec(&mut conn)
            .await?;
        Ok(())
    }

    /// Re-keys the live workspace rows from the old positional-id identity
    /// to labels: rows keyed by (or storing) a herdr id get re-keyed to the
    /// workspace's label, with the id recorded. Idempotent.
    pub(crate) async fn migrate_workspace_ids(
        &self,
        workspaces: &[crate::herdr::Workspace],
    ) -> toasty::Result<()> {
        for row in self.all_workspaces().await? {
            let Some(workspace) = workspaces.iter().find(|workspace| {
                workspace.workspace_id.as_str() == row.label
                    || workspace.workspace_id.as_str() == row.workspace_id
            }) else {
                continue;
            };
            if row.label == workspace.label && row.workspace_id == workspace.workspace_id.as_str() {
                continue;
            }
            self.delete_workspace(&row.label).await?;
            self.upsert_workspace(&WorkspaceRow {
                label: workspace.label.clone(),
                workspace_id: workspace.workspace_id.to_string(),
                forum_channel_id: row.forum_channel_id,
            })
            .await?;
        }
        Ok(())
    }

    /// Re-keys session rows from the old positional-id identity to labels:
    /// sessions whose workspace id matches a live herdr workspace get the
    /// workspace's label. Idempotent; run after
    /// [`Db::migrate_workspace_ids`].
    pub(crate) async fn migrate_session_labels(
        &self,
        workspaces: &[crate::herdr::Workspace],
    ) -> toasty::Result<()> {
        for session in self.all_sessions().await? {
            let Some(workspace) = workspaces
                .iter()
                .find(|workspace| workspace.workspace_id.as_str() == session.workspace_label)
            else {
                continue;
            };
            if session.workspace_label == workspace.label {
                continue;
            }
            let updated = SessionRow {
                session_path: session.session_path.clone(),
                post_channel_id: session.post_channel_id,
                workspace_label: workspace.label.clone(),
                cwd: session.cwd.clone(),
                transcript_path: session.transcript_path.clone(),
                synced_messages: session.synced_messages,
                last_discord_message_id: session.last_discord_message_id,
                starter_message_id: session.starter_message_id,
            };
            self.upsert_session(&updated).await?;
        }
        Ok(())
    }

    /// Returns the workspace whose forum channel is `forum_channel_id`, if
    /// any.
    pub(crate) async fn workspace_by_forum(
        &self,
        forum_channel_id: i64,
    ) -> toasty::Result<Option<WorkspaceRow>> {
        let mut conn = self.db.connection().await?;
        let row = WorkspaceRow::filter(
            WorkspaceRow::fields()
                .forum_channel_id()
                .eq(forum_channel_id),
        )
        .first()
        .exec(&mut conn)
        .await?;
        Ok(row)
    }

    /// Returns the session with the given session path, if any.
    pub(crate) async fn get_session(
        &self,
        session_path: &SessionPath,
    ) -> toasty::Result<Option<SessionRow>> {
        let mut conn = self.db.connection().await?;
        let row = SessionRow::filter(
            SessionRow::fields()
                .session_path()
                .eq(session_path.as_str()),
        )
        .first()
        .exec(&mut conn)
        .await?;
        Ok(row)
    }

    /// Inserts `row`, or replaces the session with the same session path.
    pub(crate) async fn upsert_session(&self, row: &SessionRow) -> toasty::Result<()> {
        let mut conn = self.db.connection().await?;
        SessionRow::upsert_by_session_path(&row.session_path)
            .post_channel_id(row.post_channel_id)
            .workspace_label(row.workspace_label.clone())
            .cwd(row.cwd.clone())
            .transcript_path(row.transcript_path.clone())
            .synced_messages(row.synced_messages)
            .last_discord_message_id(row.last_discord_message_id)
            .starter_message_id(row.starter_message_id)
            .exec(&mut conn)
            .await?;
        Ok(())
    }

    /// Returns the session bound to forum post `post_channel_id`, if any.
    pub(crate) async fn session_by_post(
        &self,
        post_channel_id: i64,
    ) -> toasty::Result<Option<SessionRow>> {
        let mut conn = self.db.connection().await?;
        let row = SessionRow::filter(SessionRow::fields().post_channel_id().eq(post_channel_id))
            .first()
            .exec(&mut conn)
            .await?;
        Ok(row)
    }

    /// Returns the session that reads `transcript_path`, if any — the row
    /// that adopted a rotated transcript.
    pub(crate) async fn get_session_by_transcript(
        &self,
        transcript_path: &str,
    ) -> toasty::Result<Option<SessionRow>> {
        let mut conn = self.db.connection().await?;
        let row = SessionRow::filter(SessionRow::fields().transcript_path().eq(transcript_path))
            .first()
            .exec(&mut conn)
            .await?;
        Ok(row)
    }

    /// Returns every session in `workspace_label`, in no particular order.
    pub(crate) async fn sessions_by_workspace(
        &self,
        workspace_label: &str,
    ) -> toasty::Result<Vec<SessionRow>> {
        let mut conn = self.db.connection().await?;
        let rows = SessionRow::filter(SessionRow::fields().workspace_label().eq(workspace_label))
            .exec(&mut conn)
            .await?;
        Ok(rows)
    }

    /// Returns every workspace in the database, in no particular order.
    pub(crate) async fn all_workspaces(&self) -> toasty::Result<Vec<WorkspaceRow>> {
        let mut conn = self.db.connection().await?;
        let rows = WorkspaceRow::all().exec(&mut conn).await?;
        Ok(rows)
    }

    /// Deletes the workspace with the given label.
    pub(crate) async fn delete_workspace(&self, label: &str) -> toasty::Result<()> {
        let mut conn = self.db.connection().await?;
        WorkspaceRow::filter(WorkspaceRow::fields().label().eq(label))
            .delete()
            .exec(&mut conn)
            .await?;
        Ok(())
    }

    /// Deletes the session with the given session path.
    pub(crate) async fn delete_session(&self, session_path: &str) -> toasty::Result<()> {
        let mut conn = self.db.connection().await?;
        SessionRow::filter(SessionRow::fields().session_path().eq(session_path))
            .delete()
            .exec(&mut conn)
            .await?;
        Ok(())
    }

    /// Returns every session in the database, in no particular order.
    pub(crate) async fn all_sessions(&self) -> toasty::Result<Vec<SessionRow>> {
        let mut conn = self.db.connection().await?;
        let rows = SessionRow::all().exec(&mut conn).await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::{Db, SessionPath, SessionRow, WorkspaceRow};
    use crate::herdr::{Workspace, WorkspaceId};

    #[must_use]
    fn workspace(label: &str, forum: Option<i64>) -> WorkspaceRow {
        WorkspaceRow {
            label: label.to_string(),
            workspace_id: String::new(),
            forum_channel_id: forum,
        }
    }

    #[must_use]
    fn session(path: &str, post: Option<i64>) -> SessionRow {
        SessionRow {
            session_path: path.to_string(),
            workspace_label: "herdcord".to_string(),
            cwd: "/tmp".to_string(),
            transcript_path: path.to_string(),
            post_channel_id: post,
            synced_messages: 0,
            last_discord_message_id: None,
            starter_message_id: None,
        }
    }

    #[tokio::test]
    async fn open_in_memory_and_push_schema() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        // A query against a missing table would error; None proves the
        // schema was pushed.
        assert!(
            db.get_workspace("missing")
                .await
                .expect("query works")
                .is_none()
        );
    }

    #[tokio::test]
    async fn workspace_delete_removes_row() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_workspace(&workspace("herdcord", Some(101)))
            .await
            .expect("upsert workspace");
        db.delete_workspace("herdcord")
            .await
            .expect("delete workspace");
        assert!(
            db.get_workspace("herdcord")
                .await
                .expect("query works")
                .is_none()
        );
    }

    #[tokio::test]
    async fn migrate_workspace_ids_rekeys_legacy_rows() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        // A legacy row keyed by the positional id, as stored before the
        // label-keyed identity.
        db.upsert_workspace(&WorkspaceRow {
            label: "w3".to_string(),
            workspace_id: String::new(),
            forum_channel_id: Some(101),
        })
        .await
        .expect("upsert legacy workspace");
        let workspaces = [Workspace {
            workspace_id: WorkspaceId::from("w3"),
            label: "vidhanix".to_string(),
            worktree: None,
        }];

        db.migrate_workspace_ids(&workspaces)
            .await
            .expect("migrate workspace ids");

        let row = db
            .get_workspace("vidhanix")
            .await
            .expect("query works")
            .expect("row re-keyed to label");
        assert_eq!(row.workspace_id, "w3");
        assert_eq!(row.forum_channel_id, Some(101));
        assert!(
            db.get_workspace("w3").await.expect("query works").is_none(),
            "legacy key deleted"
        );
    }

    #[tokio::test]
    async fn migrate_session_labels_rekeys_sessions() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_session(&SessionRow {
            session_path: "s1".to_string(),
            workspace_label: "w3".to_string(),
            cwd: "/tmp".to_string(),
            transcript_path: "s1".to_string(),
            post_channel_id: None,
            synced_messages: 0,
            last_discord_message_id: None,
            starter_message_id: None,
        })
        .await
        .expect("upsert legacy session");
        let workspaces = [Workspace {
            workspace_id: WorkspaceId::from("w3"),
            label: "vidhanix".to_string(),
            worktree: None,
        }];

        db.migrate_session_labels(&workspaces)
            .await
            .expect("migrate session labels");

        let row = db
            .get_session(&SessionPath::from("s1"))
            .await
            .expect("query works")
            .expect("session present");
        assert_eq!(row.workspace_label, "vidhanix");
    }

    #[tokio::test]
    async fn session_delete_removes_row() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_session(&session("s1", Some(101)))
            .await
            .expect("upsert session");
        db.delete_session("s1").await.expect("delete session");
        assert!(
            db.get_session(&SessionPath::from("s1"))
                .await
                .expect("query works")
                .is_none()
        );
    }

    #[tokio::test]
    async fn workspace_upsert_round_trips() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_workspace(&workspace("herdcord", Some(101)))
            .await
            .expect("upsert workspace");

        let got = db
            .get_workspace("herdcord")
            .await
            .expect("get workspace")
            .expect("workspace present");
        assert_eq!(got.label, "herdcord");
        assert_eq!(got.forum_channel_id, Some(101));

        // Upserting the same label replaces the row.
        db.upsert_workspace(&workspace("herdcord", Some(202)))
            .await
            .expect("upsert replaces");

        let got = db
            .get_workspace("herdcord")
            .await
            .expect("get workspace")
            .expect("workspace present");
        assert_eq!(got.forum_channel_id, Some(202));
    }

    #[tokio::test]
    async fn workspace_by_forum_finds_workspace() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_workspace(&workspace("vidhanix", None))
            .await
            .expect("upsert vidhanix");
        db.upsert_workspace(&workspace("herdcord", Some(42)))
            .await
            .expect("upsert herdcord");

        let by_forum = db
            .workspace_by_forum(42)
            .await
            .expect("query by forum")
            .expect("workspace found");
        assert_eq!(by_forum.label, "herdcord");

        // A NULL forum channel never matches a concrete snowflake.
        assert!(
            db.workspace_by_forum(999)
                .await
                .expect("query by forum")
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_upsert_round_trips_and_session_by_post() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_session(&session("s1", Some(500)))
            .await
            .expect("upsert session");

        let got = db
            .get_session(&SessionPath::from("s1"))
            .await
            .expect("get session")
            .expect("session present");
        assert_eq!(got.post_channel_id, Some(500));
        assert_eq!(got.transcript_path, "s1");
        assert_eq!(got.synced_messages, 0);
        assert_eq!(got.last_discord_message_id, None);
        assert_eq!(got.starter_message_id, None);

        let by_post = db
            .session_by_post(500)
            .await
            .expect("query by post")
            .expect("session found");
        assert_eq!(by_post.session_path, "s1");
        assert!(
            db.session_by_post(777)
                .await
                .expect("query by post")
                .is_none()
        );

        // A row that adopted a rotated transcript is found by the file it
        // reads, not just by its key.
        let mut adopted = got.clone();
        adopted.transcript_path = "new-file.jsonl".to_string();
        db.upsert_session(&adopted).await.expect("upsert adopted");
        let by_transcript = db
            .get_session_by_transcript("new-file.jsonl")
            .await
            .expect("query by transcript")
            .expect("session found");
        assert_eq!(by_transcript.session_path, "s1");
        assert!(
            db.get_session_by_transcript("unclaimed.jsonl")
                .await
                .expect("query by transcript")
                .is_none()
        );

        // Upserting an existing session updates its fields.
        let mut updated = got;
        updated.synced_messages = 3;
        db.upsert_session(&updated).await.expect("upsert updates");
        let got = db
            .get_session(&SessionPath::from("s1"))
            .await
            .expect("get session")
            .expect("session present");
        assert_eq!(got.synced_messages, 3);
    }

    #[tokio::test]
    async fn all_sessions_counts() {
        let db = Db::open_in_memory().await.expect("in-memory db opens");
        db.upsert_session(&session("a", Some(1)))
            .await
            .expect("upsert a");
        db.upsert_session(&session("b", Some(2)))
            .await
            .expect("upsert b");
        db.upsert_session(&session("c", Some(3)))
            .await
            .expect("upsert c");

        assert_eq!(db.all_sessions().await.expect("query all").len(), 3);
    }
}

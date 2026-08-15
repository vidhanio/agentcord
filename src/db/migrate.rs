//! Schema setup and migrations: pushing the toasty schema on first use,
//! adapting legacy column shapes, and re-keying rows stored under the old
//! positional-id identity.

use super::{Db, SessionRow, WorkspaceRow};
use crate::herdr::Workspace;

/// The toasty schema push is not idempotent: this checks whether the session
/// table already exists so reopening an existing database skips it.
pub(crate) async fn tables_exist(db: &toasty::Db) -> toasty::Result<bool> {
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

/// Opens (creating on first use) the database: pushes the schema derived
/// from the registered models when the tables are missing, else migrates
/// the legacy column shapes.
pub(crate) async fn open_connected(url: &str) -> toasty::Result<Db> {
    let db = toasty::Db::builder()
        .models(toasty::models!(crate::*))
        .connect(url)
        .await?;
    if tables_exist(&db).await? {
        migrate_legacy_columns(&db).await?;
    } else {
        db.push_schema().await?;
    }
    Ok(Db::from_inner(db))
}

impl Db {
    /// Re-keys the live workspace rows from the old positional-id identity
    /// to labels: rows keyed by (or storing) a herdr id get re-keyed to the
    /// workspace's label, with the id recorded. Idempotent.
    pub(crate) async fn migrate_workspace_ids(
        &self,
        workspaces: &[Workspace],
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
        workspaces: &[Workspace],
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
}

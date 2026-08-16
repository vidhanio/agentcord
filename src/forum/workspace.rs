//! The workspace↔forum lifecycle: ensuring, renaming, and re-keying a
//! workspace's forum channel, resolving worktree workspaces to their
//! repo's main workspace, workspace lookups, and pruning stale workspace
//! rows.

use std::collections::HashSet;

use serenity::all::{ChannelId, ChannelType, Context, CreateChannel, EditChannel};
use tracing::{info, warn};

use crate::{
    BotResult,
    db::WorkspaceRow,
    forum::{Forum, from_i64, titles::forum_channel_name},
    herdr::{Workspace, WorkspaceId},
};

impl Forum {
    /// Returns the forum channel for `workspace`, creating it (and
    /// persisting the mapping) on first use or when the bound forum was
    /// deleted on Discord. Every workspace gets its own forum channel,
    /// created on demand; a worktree workspace mirrors its repo's main
    /// workspace forum.
    pub async fn ensure_workspace_forum(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> BotResult<ChannelId> {
        let workspace = self.forum_workspace(workspace).await?;
        if let Some(row) = self.db.get_workspace(&workspace.label).await?
            && let Some(forum_id) = row.forum_channel_id
        {
            let forum = from_i64(forum_id)?;
            if self.channel_exists(ctx, forum).await? {
                return Ok(forum);
            }
            warn!(workspace = %workspace.label, %forum, "workspace forum deleted, re-creating");
        }

        let name = forum_channel_name(&workspace.label);
        let created = self
            .config
            .discord
            .guild_id
            .create_channel(&ctx.http, CreateChannel::new(name).kind(ChannelType::Forum))
            .await?;
        self.upsert_forum(&workspace, created.id).await?;
        info!(workspace = %workspace.label, forum = %created.id, "created workspace forum");

        Ok(created.id)
    }

    /// Ensures `workspace`'s forum exists and renames it to match the
    /// workspace's current label (channel name = sanitized label). Called on
    /// workspace events and reconcile, so renames propagate to Discord.
    pub async fn sync_workspace_forum(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> BotResult<()> {
        self.rekey_workspace(workspace).await?;
        // A worktree mirrors its main workspace's forum, which the main
        // workspace's own iteration creates and renames; only a workspace
        // with its own forum is synced here.
        if self.forum_workspace(workspace).await?.workspace_id != workspace.workspace_id {
            return Ok(());
        }
        let forum_id = self.ensure_workspace_forum(ctx, workspace).await?;
        let forum = self.forum_channel(ctx, forum_id).await?;
        let expected = forum_channel_name(&workspace.label);
        if forum.base.name.as_str() != expected {
            forum_id
                .edit(&ctx.http, EditChannel::new().name(expected.clone()))
                .await?;
            info!(workspace = %workspace.label, %expected, "renamed workspace forum");
        }
        Ok(())
    }

    /// Re-keys `workspace`'s row when it is stored under a stale key: rows
    /// from before the label-keyed identity carry the herdr id in the key
    /// position, and a renamed workspace moves its row to the new label
    /// (the stored id identifies it). No-op when the row already matches.
    async fn rekey_workspace(&self, workspace: &Workspace) -> BotResult<()> {
        if let Some(mut row) = self.db.get_workspace(&workspace.label).await? {
            if row.workspace_id != workspace.workspace_id.as_str() {
                row.workspace_id = workspace.workspace_id.to_string();
                self.db.upsert_workspace(&row).await?;
            }
            return Ok(());
        }
        let row = self
            .db
            .get_workspace_by_id(workspace.workspace_id.as_str())
            .await?
            .or(self
                .db
                .get_workspace(workspace.workspace_id.as_str())
                .await?);
        let Some(row) = row else {
            return Ok(());
        };
        self.db.delete_workspace(&row.label).await?;
        self.db
            .upsert_workspace(&WorkspaceRow {
                label: workspace.label.clone(),
                workspace_id: workspace.workspace_id.to_string(),
                forum_channel_id: row.forum_channel_id,
            })
            .await?;
        info!(
            workspace = %workspace.label,
            "re-keyed workspace row to its label"
        );
        Ok(())
    }

    /// Persists `workspace`'s forum mapping, creating the workspace row on
    /// first use.
    async fn upsert_forum(&self, workspace: &Workspace, forum_id: ChannelId) -> BotResult<()> {
        let forum_id = crate::forum::to_i64(forum_id)?;
        let row = match self.db.get_workspace(&workspace.label).await? {
            Some(mut row) => {
                row.forum_channel_id = Some(forum_id);
                row
            }
            None => WorkspaceRow {
                label: workspace.label.clone(),
                workspace_id: workspace.workspace_id.to_string(),
                forum_channel_id: Some(forum_id),
            },
        };
        self.db.upsert_workspace(&row).await?;
        Ok(())
    }

    /// The herdr workspace with `workspace_id`, if any.
    pub async fn workspace_by_id(
        &self,
        workspace_id: &WorkspaceId,
    ) -> BotResult<Option<Workspace>> {
        let workspaces = self.herdr.list_workspaces().await?;
        Ok(workspaces
            .into_iter()
            .find(|workspace| workspace.workspace_id == *workspace_id))
    }

    /// The herdr workspace with `label`, if any.
    pub async fn workspace_by_label(&self, label: &str) -> BotResult<Option<Workspace>> {
        let workspaces = self.herdr.list_workspaces().await?;
        Ok(workspaces
            .into_iter()
            .find(|workspace| workspace.label == label))
    }

    /// The branch a worktree workspace has checked out, for the starter
    /// message's `worktree` field: `None` when the workspace is not a
    /// worktree, or its branch is unknown (e.g. detached).
    pub async fn worktree_branch(&self, workspace: &Workspace) -> Option<String> {
        workspace.worktree.as_ref()?;
        let list = self
            .herdr
            .worktree_list(&workspace.workspace_id)
            .await
            .ok()?;
        list.worktrees
            .into_iter()
            .find(|entry| {
                entry.open_workspace_id.as_deref() == Some(workspace.workspace_id.as_str())
            })
            .and_then(|entry| entry.branch)
    }

    /// The main (non-worktree) workspace of the repo `workspace_id` runs
    /// in, per `worktree.list`.
    async fn worktree_source(&self, workspace_id: &WorkspaceId) -> Option<WorkspaceId> {
        let list = self.herdr.worktree_list(workspace_id).await.ok()?;
        list.source
            .and_then(|source| source.source_workspace_id)
            .map(WorkspaceId::from)
    }

    /// The workspace whose forum `workspace` mirrors: a worktree resolves
    /// to its repo's main workspace when that is open, else the worktree
    /// itself (which then gets its own forum).
    pub async fn forum_workspace(&self, workspace: &Workspace) -> BotResult<Workspace> {
        if workspace.worktree.is_none() {
            return Ok(workspace.clone());
        }
        let Some(source) = self.worktree_source(&workspace.workspace_id).await else {
            return Ok(workspace.clone());
        };
        Ok(self
            .workspace_by_id(&source)
            .await?
            .unwrap_or_else(|| workspace.clone()))
    }

    /// Deletes workspace rows whose forum was deleted and whose workspace
    /// no longer exists in herdr. A live workspace keeps its row even when
    /// its forum was deleted — `ensure_workspace_forum` re-creates it.
    pub async fn prune_stale_workspaces(&self, ctx: &Context, workspaces: &[Workspace]) {
        let live_labels = workspaces
            .iter()
            .map(|workspace| workspace.label.as_str())
            .collect::<HashSet<_>>();
        for row in self
            .db
            .all_workspaces()
            .await
            .inspect_err(|error| warn!(?error, "failed to list workspaces for pruning"))
            .unwrap_or_default()
        {
            if live_labels.contains(row.label.as_str()) {
                continue;
            }
            let Some(forum_id) = row.forum_channel_id else {
                continue;
            };
            let Ok(forum) = from_i64(forum_id) else {
                continue;
            };
            match self.channel_exists(ctx, forum).await {
                Ok(false) => {
                    info!(
                        workspace = %row.label,
                        %forum,
                        "pruning stale workspace row (forum deleted)"
                    );
                    if let Err(error) = self.db.delete_workspace(&row.label).await {
                        warn!(
                            workspace = %row.label,
                            ?error,
                            "failed to prune stale workspace row"
                        );
                    }
                }
                // Transient failure: leave the row for the next reconcile.
                Err(error) => warn!(
                    ?error,
                    workspace = %row.label,
                    "failed to check workspace forum existence"
                ),
                Ok(true) => {}
            }
        }
    }
}

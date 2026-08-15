//! Transcript polling: a fixed-tick pass that syncs every live session and
//! probes for transcript rotations.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serenity::all::Context;
use tracing::{info, warn};

use crate::{BotResult, db::SessionRow, forum::Forum, herdr::SessionPath};

/// How long a tracked transcript must stay unchanged before the poll
/// suspects the session rotated to a new file.
const SESSION_STALE_GRACE: Duration = Duration::from_secs(300);

impl Forum {
    /// Mirrors live sessions' transcripts into their posts on a fixed
    /// tick: each pass syncs every live session (cursor-based, so an
    /// unchanged file is a cheap no-op) and probes for transcript
    /// rotations. Runs in its own task so a slow sync can never stall
    /// event handling.
    pub async fn poll_loop(&self, ctx: Context) {
        let mut tick = tokio::time::interval(crate::config::MESSAGE_POLL_INTERVAL);
        loop {
            tick.tick().await;
            if let Err(error) = self.poll_once(&ctx).await {
                warn!(?error, "transcript poll pass failed");
            }
        }
    }

    /// One poll pass: sync every live session and probe each for a
    /// transcript rotation.
    async fn poll_once(&self, ctx: &Context) -> BotResult<()> {
        let keys: Vec<SessionPath> = self
            .sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .values()
            .cloned()
            .collect();
        for key in keys {
            self.sync_session_by_path(ctx, &key).await;
            self.check_rotation(ctx, &key).await;
        }
        Ok(())
    }

    /// Probes one session for a transcript rotation: when its bound file
    /// has been unchanged for a while (omp starts a new transcript when a
    /// session is replaced in the same pane, and herdr may keep reporting
    /// the old path), the session re-binds to the newest unclaimed file in
    /// its directory — the post stays, the cursor restarts.
    async fn check_rotation(&self, ctx: &Context, key: &SessionPath) {
        let Ok(Some(session)) = self.db.get_session(key).await else {
            return;
        };
        let path = PathBuf::from(&session.transcript_path);
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            return;
        };
        let stale = metadata
            .modified()
            .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > SESSION_STALE_GRACE));
        if stale && let Some(new_path) = self.rotated_session_file(&session).await {
            self.adopt_transcript(ctx, &session, &new_path).await;
        }
    }

    /// The newest transcript file in `session`'s directory when it is newer
    /// than the bound file and not claimed by another agent or session row:
    /// evidence the session rotated to a new transcript. `None` when there
    /// is nothing to adopt.
    async fn rotated_session_file(&self, session: &SessionRow) -> Option<PathBuf> {
        let bound = Path::new(&session.transcript_path);
        let dir = bound.parent()?;
        let mut entries = tokio::fs::read_dir(dir).await.ok()?;
        let mut newest: Option<(PathBuf, SystemTime)> = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if newest.as_ref().is_none_or(|(_, m)| modified > *m) {
                newest = Some((entry.path(), modified));
            }
        }
        let (candidate, _) = newest?;
        if candidate == bound {
            return None;
        }

        // A file herdr reports for any live agent belongs to that agent —
        // including a sibling whose row does not exist yet — and a file
        // another row already reads belongs to that session.
        let claimed = self
            .herdr
            .list_agents()
            .await
            .ok()?
            .into_iter()
            .filter_map(|agent| agent.agent_session)
            .map(|agent_session| agent_session.value.as_str().to_owned())
            .chain(
                self.db
                    .all_sessions()
                    .await
                    .ok()?
                    .into_iter()
                    .filter(|row| row.session_path != session.session_path)
                    .map(|row| row.transcript_path),
            )
            .collect::<HashSet<_>>();
        if claimed.contains(&candidate.to_string_lossy().into_owned()) {
            return None;
        }
        Some(candidate)
    }

    /// Re-binds a session to a new transcript file after a rotation and
    /// syncs it: the post binding and the row key (herdr's reported path)
    /// are unchanged; the cursor restarts because the new file is a fresh
    /// message stream.
    async fn adopt_transcript(&self, ctx: &Context, session: &SessionRow, new_path: &Path) {
        info!(
            session = %session.session_path,
            transcript = %new_path.display(),
            "session transcript rotated, re-binding"
        );
        let updated = SessionRow {
            workspace_label: session.workspace_label.clone(),
            session_path: session.session_path.clone(),
            cwd: session.cwd.clone(),
            transcript_path: new_path.to_string_lossy().into_owned(),
            post_channel_id: session.post_channel_id,
            synced_messages: 0,
            last_discord_message_id: None,
            starter_message_id: session.starter_message_id,
        };
        if let Err(error) = self.db.upsert_session(&updated).await {
            warn!(
                session = %session.session_path,
                ?error,
                "failed to adopt rotated transcript"
            );
            return;
        }
        self.sync_session_by_path(ctx, &SessionPath::from(session.session_path.clone()))
            .await;
    }
}

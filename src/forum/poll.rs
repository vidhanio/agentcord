//! Transcript polling: a fixed-tick pass that mirrors every live session's
//! transcript and probes for transcript rotations. One stat per session per
//! pass gates the mirror (an unchanged file is skipped entirely) and feeds
//! the rotation probe.

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
    /// tick: each pass stats every live session's bound transcript — one
    /// stat serves the mirror gate (an unchanged file skips the parse, the
    /// harness lookup, and the Discord post fetch) and the rotation probe —
    /// and mirrors the changed ones. Runs in its own task so a slow mirror
    /// can never stall event handling.
    pub async fn poll_loop(&self, ctx: Context) {
        let mut tick = tokio::time::interval(crate::config::MESSAGE_POLL_INTERVAL);
        loop {
            tick.tick().await;
            if let Err(error) = self.poll_once(&ctx).await {
                warn!(?error, "transcript poll pass failed");
            }
        }
    }

    /// One poll pass: for each live session, stat its bound transcript and
    /// mirror it when the stamp changed since the last pass; then probe it
    /// for a rotation.
    async fn poll_once(&self, ctx: &Context) -> BotResult<()> {
        let keys: Vec<SessionPath> = self
            .sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .values()
            .cloned()
            .collect();
        for key in keys {
            let Some((session, stamp)) = self.transcript_stamp(&key).await else {
                continue;
            };
            let unchanged = {
                let mut stamps = self
                    .transcript_stamps
                    .lock()
                    .expect("transcript_stamps lock poisoned");
                // An unknown stamp (unreadable mtime) mirrors, to be safe.
                stamp.is_some_and(|stamp| {
                    let unchanged = stamps.get(&key).is_some_and(|known| *known == stamp);
                    stamps.insert(key.clone(), stamp);
                    unchanged
                })
            };
            if !unchanged {
                // A changed file is mirrored; the recovery escalation
                // inside `sync_session_by_path` re-creates a deleted post.
                // An unchanged file's deleted post is left to the
                // reconcile, whose ensure pass re-creates it.
                self.sync_session_by_path(ctx, &key).await;
            }
            self.check_rotation(ctx, &session, stamp).await;
        }
        Ok(())
    }

    /// The session row and the stamp (mtime, size) of its bound
    /// transcript, when both exist. A missing file (mid-rotation
    /// delete+recreate dance) skips the session entirely — the mirror would
    /// no-op on it and the rotation probe needs the stamp.
    async fn transcript_stamp(
        &self,
        key: &SessionPath,
    ) -> Option<(SessionRow, Option<(SystemTime, u64)>)> {
        let session = self.db.get_session(key).await.ok()??;
        let metadata = tokio::fs::metadata(&session.transcript_path).await.ok()?;
        Some((
            session,
            metadata
                .modified()
                .ok()
                .map(|modified| (modified, metadata.len())),
        ))
    }

    /// Probes one session for a transcript rotation: when its bound file
    /// has been unchanged for a while (omp starts a new transcript when a
    /// session is replaced in the same pane, and herdr may keep reporting
    /// the old path), the session re-binds to the newest unclaimed file in
    /// its directory — the post stays, the cursor restarts.
    async fn check_rotation(
        &self,
        ctx: &Context,
        session: &SessionRow,
        stamp: Option<(SystemTime, u64)>,
    ) {
        let Some((modified, _)) = stamp else {
            return;
        };
        if !modified
            .elapsed()
            .is_ok_and(|age| age > SESSION_STALE_GRACE)
        {
            return;
        }
        if let Some(new_path) = self.rotated_session_file(session).await {
            self.adopt_transcript(ctx, session, &new_path).await;
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

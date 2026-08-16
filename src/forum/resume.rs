//! Resuming dead sessions: re-launching the agent of a dead session in its
//! workspace with a native harness resume, re-binding it to the same post.

use serenity::all::Context;
use tracing::{info, warn};

use crate::{
    BotResult,
    db::SessionRow,
    error::BotError,
    forum::{Forum, from_i64},
    herdr::{Agent, SessionPath},
    session::Harness,
};

impl Forum {
    /// Re-launches the agent of a dead session in its workspace, resuming
    /// the same conversation (native harness resume: `omp --resume=<path>`,
    /// `claude --resume <id>`, `codex resume <id>`), and re-binds the
    /// session to its post. `None` when the session is already being
    /// resumed by a concurrent message.
    pub async fn resume_session(
        &self,
        ctx: &Context,
        session: &SessionRow,
    ) -> BotResult<Option<Agent>> {
        let key = SessionPath::from(session.session_path.clone());
        {
            let mut resuming = self.resuming.lock().expect("resuming lock poisoned");
            if !resuming.insert(key.clone()) {
                return Ok(None);
            }
        }
        let result = self.resume_session_inner(ctx, session).await;
        self.resuming
            .lock()
            .expect("resuming lock poisoned")
            .remove(&key);
        result.map(Some)
    }

    async fn resume_session_inner(&self, ctx: &Context, session: &SessionRow) -> BotResult<Agent> {
        let post_id = session.post_channel_id.ok_or_else(|| {
            BotError::Other(format!(
                "session `{}` has no forum post",
                session.session_path
            ))
        })?;
        let post = from_i64(post_id)?;
        let harness = self
            .applied_harness(ctx, post)
            .await?
            .unwrap_or(crate::config::DEFAULT_HARNESS);
        let args = resume_args(harness, session);
        let workspace = self.workspace_by_label(&session.workspace_label).await?;
        let name = self.fresh_agent_name().await?;

        info!(
            session = %session.session_path,
            %name,
            harness = harness.as_str(),
            "resuming session in a new agent"
        );

        // The new workspace's label when the old one is gone: a fresh
        // workspace named after the agent.
        let workspace_label = workspace
            .as_ref()
            .map_or_else(|| name.clone(), |workspace| workspace.label.clone());
        let started = match workspace {
            Some(workspace) => {
                self.spawn_in_workspace(&workspace, &name, harness, &session.cwd, &args)
                    .await?
            }
            None => {
                self.spawn_in_new_workspace(&name, &name, harness, &session.cwd, &args)
                    .await?
            }
        };

        // The row follows the agent into its (possibly re-created)
        // workspace; the post binding, transcript, and sync cursor are
        // untouched — a native resume continues the same transcript.
        let updated = SessionRow {
            workspace_label,
            session_path: session.session_path.clone(),
            cwd: started.cwd.to_string_lossy().into_owned(),
            transcript_path: session.transcript_path.clone(),
            post_channel_id: session.post_channel_id,
            synced_messages: session.synced_messages,
            last_discord_message_id: session.last_discord_message_id,
            starter_message_id: session.starter_message_id,
        };
        self.db.upsert_session(&updated).await?;
        self.ensure_session_post(ctx, &started).await?;

        // The post's starter message carries the pane id, which the new
        // agent changed; refresh it so the preview shows the resumed pane.
        let key = SessionPath::from(session.session_path.clone());
        if let Ok(Some(row)) = self.db.get_session(&key).await
            && let Some(post_id) = row.post_channel_id
            && let Ok(post) = from_i64(post_id)
            && let Err(error) = self.refresh_agent_intro(ctx, &row, post, &started).await
        {
            warn!(
                ?error,
                session = %session.session_path,
                "failed to refresh resumed session intro"
            );
        }
        Ok(started)
    }
}

/// The `agent.start` arguments that resume `session`'s conversation in its
/// harness: omp resumes by transcript path, claude-code and codex by
/// session id (the row key — herdr's reported session reference), pi and
/// opencode by session id via `--session`.
#[must_use]
fn resume_args(harness: Harness, session: &SessionRow) -> Vec<String> {
    match harness {
        Harness::Omp => vec![format!("--resume={}", session.transcript_path)],
        Harness::ClaudeCode => vec!["--resume".into(), session.session_path.clone()],
        Harness::Codex => vec!["resume".into(), session.session_path.clone()],
        Harness::Pi | Harness::Opencode => {
            vec!["--session".into(), session.session_path.clone()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resume_args;
    use crate::{db::SessionRow, session::Harness};

    #[test]
    fn resume_args_resume_by_harness() {
        let session = SessionRow {
            session_path: "s1".to_owned(),
            workspace_label: "w1".to_owned(),
            cwd: "/tmp".to_owned(),
            transcript_path: "/tmp/s1.jsonl".to_owned(),
            post_channel_id: Some(1),
            synced_messages: 0,
            last_discord_message_id: None,
            starter_message_id: None,
        };
        assert_eq!(
            resume_args(Harness::Omp, &session),
            vec!["--resume=/tmp/s1.jsonl"]
        );
        assert_eq!(
            resume_args(Harness::ClaudeCode, &session),
            vec!["--resume", "s1"]
        );
        assert_eq!(resume_args(Harness::Codex, &session), vec!["resume", "s1"]);
        assert_eq!(resume_args(Harness::Pi, &session), vec!["--session", "s1"]);
        assert_eq!(
            resume_args(Harness::Opencode, &session),
            vec!["--session", "s1"]
        );
    }
}

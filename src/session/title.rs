//! Transcript-sourced session titles.

use std::{io::BufRead, path::Path};

use serde_json::Value;

use super::Harness;

/// Reads the session's own title from its transcript, when the harness
/// records one. Stable — unlike herdr's terminal title, which animates —
/// so the post title only changes when the task does.
///
/// - `omp`: `{"type":"title","title":…}` (new) /
///   `{"type":"title_change","title":…}` (legacy) records; the last one wins.
/// - `claude-code`: `custom-title` (user-set), `ai-title` (auto), or the
///   first-line `summary`, in that priority.
/// - `pi`: `{"type":"session_info","name":…}` records; the last one wins.
/// - `opencode`: the store's `session.title` column for the session id (the
///   path's string form).
/// - `codex`: no title record; `None`.
///
/// `None` when the source is missing or no usable title exists yet.
#[must_use]
pub fn read_session_title(harness: Harness, path: &Path) -> Option<String> {
    if harness == Harness::Codex {
        return None;
    }
    if harness == Harness::Opencode {
        let session_id = path.to_string_lossy();
        return super::opencode::open_opencode_db()
            .ok()
            .and_then(|conn| super::opencode::read_opencode_title(&conn, &session_id));
    }
    let file = std::fs::File::open(path).ok()?;
    let mut title: Option<String> = None;
    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match (harness, value.get("type").and_then(Value::as_str)) {
            (Harness::Omp, Some("title" | "title_change")) => {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (Harness::Pi, Some("session_info")) => {
                title = value.get("name").and_then(Value::as_str).map(str::to_owned);
            }
            (Harness::ClaudeCode, Some("custom-title")) => {
                custom = value
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (Harness::ClaudeCode, Some("ai-title")) => {
                ai = value
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (Harness::ClaudeCode, Some("summary")) => {
                title = value
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    let chosen = match harness {
        Harness::ClaudeCode => custom.or(ai).or(title),
        _ => title,
    };
    chosen
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::read_session_title;
    use crate::session::Harness;

    #[test]
    fn session_title_omp_last_wins_and_trims() {
        let path = std::env::temp_dir().join(format!("herdcord-title-omp-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"type":"session","version":3,"id":"s"}
{"type":"title","v":1,"title":"First task","source":"auto"}
{"type":"message","id":"m1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"title_change","title":"  Second task  "}
"#,
        )
        .unwrap();
        assert_eq!(
            read_session_title(Harness::Omp, &path).as_deref(),
            Some("Second task")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn session_title_claude_prefers_custom_over_ai_over_summary() {
        let path =
            std::env::temp_dir().join(format!("herdcord-title-claude-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"type":"summary","summary":"A summary title"}
{"type":"ai-title","sessionId":"s","aiTitle":"Auto title"}
{"type":"custom-title","customTitle":"My Title","sessionId":"s"}
"#,
        )
        .unwrap();
        assert_eq!(
            read_session_title(Harness::ClaudeCode, &path).as_deref(),
            Some("My Title")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn session_title_claude_falls_back_to_ai_and_summary() {
        let path =
            std::env::temp_dir().join(format!("herdcord-title-claude2-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"type":"summary","summary":"A summary title"}
{"type":"ai-title","sessionId":"s","aiTitle":"Auto title"}
"#,
        )
        .unwrap();
        assert_eq!(
            read_session_title(Harness::ClaudeCode, &path).as_deref(),
            Some("Auto title")
        );
        std::fs::write(&path, r#"{"type":"summary","summary":"A summary title"}"#).unwrap();
        assert_eq!(
            read_session_title(Harness::ClaudeCode, &path).as_deref(),
            Some("A summary title")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn session_title_codex_and_missing_are_none() {
        let path =
            std::env::temp_dir().join(format!("herdcord-title-codex-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"type":"response_item","payload":{"type":"message"}}"#,
        )
        .unwrap();
        assert_eq!(read_session_title(Harness::Codex, &path), None);
        std::fs::remove_file(&path).ok();
        let missing =
            std::env::temp_dir().join(format!("herdcord-title-missing-{}", std::process::id()));
        assert_eq!(read_session_title(Harness::Omp, &missing), None);
    }
}

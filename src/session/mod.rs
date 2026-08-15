//! Normalized session transcripts for the supported agent harnesses.
//!
//! Each harness writes its own JSONL session format under a different path
//! (`~/.omp/agent/sessions/...`, `~/.claude/projects/...`,
//! `~/.codex/sessions/...`); [`read_session`] parses any of them into a
//! common [`SessionMessage`] stream. `opencode` is the exception: its
//! sessions live in a SQLite store and are read via [`read_session_messages`].
//!
//! The conversation model lives in `model`, the per-harness parsers in
//! `omp`/`claude`/`codex`/`pi`/`opencode`, the shared parsing skeleton in
//! `common`, and the transcript-sourced titles in `title`.

use std::{io::Result as IoResult, path::Path};

mod claude;
mod codex;
mod common;
mod model;
mod omp;
mod opencode;
mod pi;
mod title;

use self::{claude::parse_claude_code, codex::parse_codex, omp::parse_omp, pi::parse_pi};
pub use self::{
    common::cap,
    model::{SessionMessage, SessionRole, ToolCall, ToolCallId, ToolState},
    title::read_session_title,
};

/// The supported harnesses (strict enum — no free strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    /// The `oh-my-pi` (`omp`) harness.
    Omp,
    /// Anthropic's Claude Code CLI.
    ClaudeCode,
    /// `Codex` CLI.
    Codex,
    /// The `pi` agent CLI.
    Pi,
    /// The `opencode` agent CLI.
    Opencode,
}

impl Harness {
    /// All supported harnesses, in canonical order.
    pub const ALL: [Self; 5] = [
        Self::Omp,
        Self::ClaudeCode,
        Self::Codex,
        Self::Pi,
        Self::Opencode,
    ];

    /// The canonical identifier used in session paths and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
        }
    }

    /// Parses a harness from a string, case-insensitively.
    ///
    /// In addition to the canonical identifiers, `claude` and `claude_code`
    /// are accepted as aliases for [`Harness::ClaudeCode`]. `pi` and
    /// `opencode` take no aliases.
    #[must_use]
    pub const fn parse(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("omp") {
            return Some(Self::Omp);
        }
        if s.eq_ignore_ascii_case("claude-code")
            || s.eq_ignore_ascii_case("claude")
            || s.eq_ignore_ascii_case("claude_code")
        {
            return Some(Self::ClaudeCode);
        }
        if s.eq_ignore_ascii_case("codex") {
            return Some(Self::Codex);
        }
        if s.eq_ignore_ascii_case("pi") {
            return Some(Self::Pi);
        }
        if s.eq_ignore_ascii_case("opencode") {
            return Some(Self::Opencode);
        }
        None
    }

    /// A human-friendly display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Omp => "Omp",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::Opencode => "OpenCode",
        }
    }
}

/// Reads and normalizes a session transcript into a conversation log.
///
/// Lines are processed in file order. Malformed lines and lines whose text is
/// empty or whitespace-only are skipped, and a truncated final line is
/// tolerated. Synchronous: transcript files are small, so callers may run
/// this on a Tokio task.
///
/// `Opencode` sessions live in a SQLite store rather than a transcript file,
/// so this returns [`std::io::ErrorKind::Unsupported`] for them — use
/// [`read_session_messages`] instead.
pub fn read_session(harness: Harness, path: &Path) -> IoResult<Vec<SessionMessage>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(match harness {
        Harness::Omp => parse_omp(&raw),
        Harness::ClaudeCode => parse_claude_code(&raw),
        Harness::Codex => parse_codex(&raw),
        Harness::Pi => parse_pi(&raw),
        Harness::Opencode => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "opencode sessions are store-backed",
            ));
        }
    })
}

/// Reads a session's messages by harness.
///
/// `Opencode` sessions come from their SQLite store, keyed by session id
/// (`path_or_id` is the store's session id); every other harness comes from
/// its transcript file (`path_or_id` is the file path), with the same
/// semantics as [`read_session`].
pub fn read_session_messages(harness: Harness, path_or_id: &str) -> IoResult<Vec<SessionMessage>> {
    if harness == Harness::Opencode {
        return opencode::open_opencode_db()
            .and_then(|conn| opencode::read_opencode_session(&conn, path_or_id));
    }
    read_session(harness, Path::new(path_or_id))
}

#[cfg(test)]
mod tests {
    use super::{Harness, read_session};

    #[test]
    fn harness_parse_round_trips() {
        for harness in Harness::ALL {
            assert_eq!(Harness::parse(harness.as_str()), Some(harness));
        }
        assert_eq!(Harness::Omp.as_str(), "omp");
        assert_eq!(Harness::ClaudeCode.as_str(), "claude-code");
        assert_eq!(Harness::Codex.as_str(), "codex");
        assert_eq!(Harness::Pi.as_str(), "pi");
        assert_eq!(Harness::Opencode.as_str(), "opencode");
        assert_eq!(Harness::Omp.label(), "Omp");
        assert_eq!(Harness::ClaudeCode.label(), "Claude Code");
        assert_eq!(Harness::Codex.label(), "Codex");
        assert_eq!(Harness::Pi.label(), "Pi");
        assert_eq!(Harness::Opencode.label(), "OpenCode");
        assert_eq!(Harness::ALL.len(), 5);

        assert_eq!(Harness::parse("OMP"), Some(Harness::Omp));
        assert_eq!(Harness::parse("OmP"), Some(Harness::Omp));
        assert_eq!(Harness::parse("CLAUDE-CODE"), Some(Harness::ClaudeCode));
        assert_eq!(Harness::parse("claude"), Some(Harness::ClaudeCode));
        assert_eq!(Harness::parse("claude_code"), Some(Harness::ClaudeCode));
        assert_eq!(Harness::parse("Claude_Code"), Some(Harness::ClaudeCode));
        assert_eq!(Harness::parse("CODEX"), Some(Harness::Codex));
        assert_eq!(Harness::parse("pi"), Some(Harness::Pi));
        assert_eq!(Harness::parse("PI"), Some(Harness::Pi));
        assert_eq!(Harness::parse("Pi"), Some(Harness::Pi));
        assert_eq!(Harness::parse("opencode"), Some(Harness::Opencode));
        assert_eq!(Harness::parse("OPENCODE"), Some(Harness::Opencode));
        assert_eq!(Harness::parse("OpenCode"), Some(Harness::Opencode));

        assert_eq!(Harness::parse(""), None);
        assert_eq!(Harness::parse("gpt"), None);
        assert_eq!(Harness::parse(" claude-code "), None);
        assert_eq!(Harness::parse("open_code"), None);
    }

    #[test]
    fn read_session_reads_file() {
        let path =
            std::env::temp_dir().join(format!("herdcord-session-test-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"type":"message","id":"m1","timestamp":"2026-08-12T22:22:58.354Z","message":{"role":"user","content":"hello"}}
"#,
        )
        .unwrap();
        let messages = read_session(Harness::Omp, &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello");

        let missing = std::env::temp_dir().join("herdcord-session-does-not-exist.jsonl");
        assert!(read_session(Harness::Omp, &missing).is_err());
    }
}

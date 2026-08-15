//! Normalized session transcripts for the supported agent harnesses.
//!
//! Each harness writes its own JSONL session format under a different path
//! (`~/.omp/agent/sessions/...`, `~/.claude/projects/...`,
//! `~/.codex/sessions/...`); [`read_session`] parses any of them into a
//! common [`SessionMessage`] stream. `opencode` is the exception: its
//! sessions live in a SQLite store and are read via [`read_session_messages`].

use std::{
    fmt::{self, Display, Formatter},
    io::{BufRead, Result as IoResult},
    path::Path,
};

use nutype::nutype;
use serde_json::Value;

mod claude;
mod codex;
mod common;
mod omp;
mod opencode;
mod pi;

pub use self::common::cap;
use self::{
    claude::parse_claude_code, codex::parse_codex, common::TOOL_TEXT_LIMIT, omp::parse_omp,
    pi::parse_pi,
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

/// One side of a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionRole {
    /// The human user.
    User,
    /// The agent harness.
    Agent,
    /// A tool call (name + arguments), not a conversation turn.
    Tool,
}

/// Lifecycle state of a tool call: running until its completion record
/// appears, then done or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolState {
    /// No completion record for the call yet.
    Running,
    /// The call completed without an error.
    Done,
    /// The call's completion record reports an error.
    Failed,
}

impl Display for ToolState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        })
    }
}

/// A tool call id as recorded in a transcript, e.g. `"call_0"` or
/// `"tu_1"`; pairs a call with its completion record and its posted embed.
#[nutype(
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        Deref,
        Default,
        From,
        Serialize,
        Deserialize
    ),
    default = ""
)]
pub struct ToolCallId(String);

/// A tool call as recorded in the transcript: the call (name + arguments)
/// paired with its completion record when present.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCall {
    /// The harness's call id, used to pair the call with its completion
    /// and to find the posted embed for in-place edits.
    pub call_id: ToolCallId,
    /// The tool's name.
    pub name: String,
    /// Compact-JSON arguments (full — display caps them); `None` when the
    /// call took no arguments.
    pub args: Option<String>,
    /// Whether the call is still running or has completed, computed from
    /// the transcript's completion records.
    pub state: ToolState,
    /// The tool's error text on failure, capped at [`TOOL_TEXT_LIMIT`]
    /// characters.
    pub error: Option<String>,
}

impl Display for ToolCall {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let full = self
            .args
            .as_ref()
            .map_or_else(|| self.name.clone(), |args| format!("{} {args}", self.name));
        f.write_str(&cap(&full, TOOL_TEXT_LIMIT))
    }
}

/// A normalized conversation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMessage {
    /// Which side of the conversation produced this message.
    pub role: SessionRole,
    /// The message text.
    pub text: String,
    /// Structured tool-call data when `role` is Tool.
    pub tool: Option<ToolCall>,
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
        return opencode::open_opencode_db()
            .ok()
            .and_then(|conn| opencode::read_opencode_title(&conn, &session_id));
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
    use super::{
        Harness, SessionRole, TOOL_TEXT_LIMIT, ToolCallId, ToolState, claude::parse_claude_code,
        codex::parse_codex, omp::parse_omp, read_session, read_session_title,
    };

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
    fn omp_messages_parsed() {
        let raw = r#"{"type":"title","v":1,"title":"test"}
{"type":"session","version":3,"id":"019ff7ed","timestamp":"2026-08-12T21:42:48.300Z","cwd":"/home/vidhanio/Projects/herdcord"}
{"type":"custom","customType":"goal-mode-context","content":"<goal_context>"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-12T22:22:58.354Z","message":{"role":"user","content":[{"type":"text","text":"create a discord bot"}]}}
{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-12T22:23:03.982Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me start"},{"type":"text","text":"I will inspect the files."},{"type":"toolCall","name":"read","arguments":{}}]}}
{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-08-12T22:23:03.990Z","message":{"role":"toolResult","toolCallId":"call_0","toolName":"read","content":[{"type":"text","text":"---\nname: herdr"}]}}
{"type":"mode_change","id":"x","parentId":null,"timestamp":"2026-08-12T22:22:58.290Z","mode":"goal"}
{"type":"title_change","title":"new title"}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "create a discord bot");
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "I will inspect the files.");
    }

    #[test]
    fn omp_tool_calls_parsed() {
        let raw = r#"{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_00_1","toolName":"read","startedAt":"2026-08-12T22:23:03.982Z","args":{"path":"src/forum.rs:480-745"},"intent":"read a range"},"id":"i1","parentId":"m2","timestamp":"2026-08-12T22:23:03.982Z"}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_00_2","toolName":"ask","startedAt":"2026-08-12T22:23:04.000Z","intent":"ask the user"},"id":"i2","parentId":"m2","timestamp":"2026-08-12T22:23:04.000Z"}
{"type":"custom","customType":"goal-mode-context","content":"<goal_context>"}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::Tool);
        assert_eq!(messages[0].text, "read {\"path\":\"src/forum.rs:480-745\"}");
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_00_1"));
        assert_eq!(call.name, "read");
        assert_eq!(
            call.args.as_deref(),
            Some(r#"{"path":"src/forum.rs:480-745"}"#)
        );
        // No completion record in the file: still running.
        assert_eq!(call.state, ToolState::Running);
        assert_eq!(call.error, None);
        assert_eq!(messages[1].role, SessionRole::Tool);
        assert_eq!(messages[1].text, r#"ask "ask the user""#);
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_00_2"));
        assert_eq!(call.name, "ask");
        // No arguments recorded: the intent stands in as the argument.
        assert_eq!(call.args.as_deref(), Some(r#""ask the user""#));
        assert_eq!(call.state, ToolState::Running);
    }

    #[test]
    fn omp_tools_without_args_fall_back_to_intent() {
        let raw = r#"{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_0","toolName":"hub","startedAt":"2026-08-12T22:39:06.280Z","intent":"Checking subagent status"},"id":"i1","parentId":"m2","timestamp":"2026-08-12T22:39:06.280Z"}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_1","toolName":"task","startedAt":"2026-08-12T22:31:44.175Z","intent":"Delegating nix/CI scaffolding"},"id":"i2","timestamp":"2026-08-12T22:31:44.175Z"}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_2","toolName":"ask","startedAt":"2026-08-12T22:23:04.000Z"},"id":"i3","timestamp":"2026-08-12T22:23:04.000Z"}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_3","toolName":"read","args":{"path":"src"},"intent":"read a file"},"id":"i4","timestamp":"2026-08-12T22:23:05.000Z"}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 4);
        // No arguments recorded: the intent stands in as the argument.
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.name, "hub");
        assert_eq!(call.args.as_deref(), Some(r#""Checking subagent status""#));
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.name, "task");
        assert_eq!(
            call.args.as_deref(),
            Some(r#""Delegating nix/CI scaffolding""#)
        );
        // No intent either: no arguments, as before.
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.name, "ask");
        assert_eq!(call.args, None);
        // Real arguments win over the intent.
        let call = messages[3].tool.as_ref().unwrap();
        assert_eq!(call.name, "read");
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src"}"#));
    }

    #[test]
    fn omp_tool_calls_anchor_to_results() {
        let blob = "x".repeat(500);
        let raw = format!(
            r#"{{"type":"custom","customType":"tool_execution_start","data":{{"toolCallId":"call_0","toolName":"read","args":{{"path":"src"}}}},"id":"i1","timestamp":"2026-08-12T22:23:03.982Z"}}
{{"type":"custom","customType":"tool_execution_start","data":{{"toolCallId":"call_1","toolName":"write","args":{{"path":"out.txt"}}}},"id":"i2","timestamp":"2026-08-12T22:23:04.000Z"}}
{{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-08-12T22:23:05.000Z","message":{{"role":"toolResult","toolCallId":"call_0","toolName":"read","content":[{{"type":"text","text":"ok"}}]}}}}
{{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-08-12T22:23:06.000Z","message":{{"role":"toolResult","toolCallId":"call_1","toolName":"write","isError":true,"content":[{{"type":"text","text":"{blob}"}}]}}}}"#
        );
        let messages = parse_omp(&raw);
        assert_eq!(messages.len(), 2);
        // Completed without an error.
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_0"));
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src"}"#));
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        assert_eq!(messages[0].text, "read {\"path\":\"src\"}");
        // Failed, error capped at TOOL_TEXT_LIMIT characters.
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_1"));
        assert_eq!(call.state, ToolState::Failed);
        let error = call.error.as_deref().unwrap();
        assert_eq!(error.chars().count(), TOOL_TEXT_LIMIT + 1);
        assert!(error.ends_with('…'));
    }

    #[test]
    fn omp_tool_args_truncated_at_cap() {
        let blob = "x".repeat(500);
        let raw = format!(
            r#"{{"type":"custom","customType":"tool_execution_start","data":{{"toolCallId":"call_00_3","toolName":"read","startedAt":"2026-08-12T22:23:05.000Z","args":{{"content":"{blob}"}},"intent":"read a file"}},"id":"i3","parentId":"m2","timestamp":"2026-08-12T22:23:05.000Z"}}"#
        );
        let messages = parse_omp(&raw);
        assert_eq!(messages.len(), 1);
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.name, "read");
        // Arguments stay full (the embed splits them per field); only the
        // text display is capped.
        let args = call.args.as_deref().unwrap();
        assert!(args.starts_with("{\"content\":\""));
        assert!(args.ends_with("\"}"));
        assert!(messages[0].text.starts_with("read "));
        assert_eq!(messages[0].text.chars().count(), TOOL_TEXT_LIMIT + 1);
        assert!(messages[0].text.ends_with('…'));
    }

    #[test]
    fn omp_empty_whitespace_dropped() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-12T22:22:58.354Z","message":{"role":"user","content":[{"type":"text","text":"   "}]}}
{"type":"message","id":"m2","timestamp":"2026-08-12T22:22:59.000Z","message":{"role":"assistant","content":[]}}
{"type":"message","id":"m3","timestamp":"2026-08-12T22:23:00.000Z","message":{"role":"user","content":[{"type":"thinking","thinking":"only thinking"}]}}
{"type":"message","id":"m4","timestamp":"2026-08-12T22:23:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"real reply"}]}}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "real reply");
    }

    #[test]
    fn claude_messages_parsed() {
        let raw = r#"{"type":"summary","timestamp":"2025-12-01T09:00:00Z","summary":"This is a summary of prior turns."}
{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user","content":"Hello Claude"}}
{"type":"assistant","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Hello! How can I help?"},{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"src/main.ts","offset":10}}]}}
{"type":"file-history-snapshot","timestamp":"2025-12-01T10:00:02Z","fileHistory":[]}
{"type":"user","timestamp":"2025-12-01T10:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ls output"}]}}
"#;
        let messages = parse_claude_code(raw);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "Hello Claude");
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "Hello! How can I help?");
        assert_eq!(messages[2].role, SessionRole::Tool);
        assert_eq!(
            messages[2].text,
            "Read {\"file_path\":\"src/main.ts\",\"offset\":10}"
        );
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("tu_1"));
        assert_eq!(
            call.args.as_deref(),
            Some(r#"{"file_path":"src/main.ts","offset":10}"#)
        );
        // The transcript carries a clean `tool_result` for this call.
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
    }

    #[test]
    fn claude_tool_calls_anchor_to_results() {
        let raw = r#"{"type":"assistant","timestamp":"2025-12-01T10:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"src/main.ts"}}]}}
{"type":"assistant","timestamp":"2025-12-01T10:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_2","name":"Edit","input":{"file_path":"src/main.ts","old_string":"a","new_string":"b"}}]}}
{"type":"assistant","timestamp":"2025-12-01T10:00:03Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_3","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","timestamp":"2025-12-01T10:00:04Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ls output"}]}}
{"type":"user","timestamp":"2025-12-01T10:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_3","is_error":true,"content":[{"type":"text","text":"command failed"}]}]}}
"#;
        let messages = parse_claude_code(raw);
        // Only the three tool calls: results carry no conversation text.
        assert_eq!(messages.len(), 3);
        // Done: clean tool_result present.
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("tu_1"));
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        assert_eq!(messages[0].text, "Read {\"file_path\":\"src/main.ts\"}");
        // Running: no tool_result for this call.
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("tu_2"));
        assert_eq!(call.state, ToolState::Running);
        assert_eq!(call.error, None);
        // Failed: tool_result with is_error.
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("tu_3"));
        assert_eq!(call.state, ToolState::Failed);
        assert_eq!(call.error.as_deref(), Some("command failed"));
    }

    #[test]
    fn claude_top_level_content_fallback() {
        let raw = r#"{"type":"user","timestamp":"2025-12-01T10:00:00Z","message":{"role":"user"},"content":"hello from top level"}
"#;
        let messages = parse_claude_code(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello from top level");
    }

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

    #[test]
    fn codex_messages_parsed() {
        let raw = r#"{"timestamp":"2026-06-28T10:00:00.000Z","type":"session_meta","payload":{"id":"sess_1"}}
{"timestamp":"2026-06-28T10:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"please inspect the files"}}
{"timestamp":"2026-06-28T10:00:02.000Z","type":"response_item","payload":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"I will inspect the files."}]}}
{"timestamp":"2026-06-28T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"src\"}"}}
{"timestamp":"2026-06-28T10:00:04.000Z","type":"token_count","payload":{"input_tokens":100,"output_tokens":10}}
{"timestamp":"2026-06-28T10:00:05.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"encrypted"}]}}
"#;
        let messages = parse_codex(raw);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "please inspect the files");
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "I will inspect the files.");
        assert_eq!(messages[2].role, SessionRole::Tool);
        assert_eq!(messages[2].text, "read {\"path\":\"src\"}");
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_1"));
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src"}"#));
        // No completion record in the file: still running.
        assert_eq!(call.state, ToolState::Running);
        assert_eq!(call.error, None);
    }

    #[test]
    fn codex_tool_calls_anchor_to_results() {
        let raw = r#"{"timestamp":"2026-06-28T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"src\"}"}}
{"timestamp":"2026-06-28T10:00:04.000Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_2","name":"ask","arguments":"{\"question\":\"ok?\"}"}}
{"timestamp":"2026-06-28T10:00:05.000Z","type":"response_item","payload":{"type":"function_call","call_id":"call_3","name":"write","arguments":"{\"path\":\"out\"}"}}
{"timestamp":"2026-06-28T10:00:06.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"file contents"}}
{"timestamp":"2026-06-28T10:00:07.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_2","output":"user declined","is_error":true}}
"#;
        let messages = parse_codex(raw);
        assert_eq!(messages.len(), 3);
        // Done: clean output present.
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_1"));
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src"}"#));
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        assert_eq!(messages[0].text, "read {\"path\":\"src\"}");
        // Failed: output with is_error.
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_2"));
        assert_eq!(call.state, ToolState::Failed);
        assert_eq!(call.error.as_deref(), Some("user declined"));
        // Running: no output record.
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_3"));
        assert_eq!(call.state, ToolState::Running);
        assert_eq!(call.error, None);
    }

    #[test]
    fn omp_truncated_execution_args_fall_back_to_message_arguments() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-14T23:00:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"bash","arguments":{"command":"ls -la /very/long/path/with/plenty/of/segments"}}]}}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_0","toolName":"bash","args":{"command":"ls -la /very/long…"}},"id":"i1","timestamp":"2026-08-14T23:00:01.000Z"}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 1);
        let call = messages[0].tool.as_ref().unwrap();
        // The record's args were truncated by omp (the trailing ellipsis);
        // the full arguments come from the message record.
        assert_eq!(
            call.args.as_deref(),
            Some(r#"{"command":"ls -la /very/long/path/with/plenty/of/segments"}"#)
        );
        assert_eq!(
            messages[0].text,
            "bash {\"command\":\"ls -la /very/long/path/with/plenty/of/segments\"}"
        );
    }

    #[test]
    fn omp_untruncated_args_keep_the_record_values() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-14T23:00:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"bash","arguments":{"command":"echo full version"}}]}}
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"call_0","toolName":"bash","args":{"command":"echo short"}},"id":"i1","timestamp":"2026-08-14T23:00:01.000Z"}
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 1);
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.args.as_deref(), Some(r#"{"command":"echo short"}"#));
    }

    #[test]
    fn malformed_lines_skipped() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-12T22:22:58.354Z","message":{"role":"user","content":"first"}}
not json at all
{"type":"message","id":"m2","timestamp":"2026-08-12T22:22:59.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}
{ broken json
"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "first");
        assert_eq!(messages[1].text, "second");
    }

    #[test]
    fn truncated_final_line() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-12T22:22:58.354Z","message":{"role":"user","content":"first"}}
{"type":"message","id":"m2","timestamp":"2026-08-12T22:22:59.000Z","message":{"role":"assistant","content":"trunc"#;
        let messages = parse_omp(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "first");
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

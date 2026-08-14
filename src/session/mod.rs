//! Normalized session transcripts for the supported agent harnesses.
//!
//! Each harness writes its own JSONL session format under a different path
//! (`~/.omp/agent/sessions/...`, `~/.claude/projects/...`,
//! `~/.codex/sessions/...`); [`read_session`] parses any of them into a
//! common [`SessionMessage`] stream.

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

pub use self::common::cap;
use self::{
    claude::parse_claude_code, codex::parse_codex, common::TOOL_TEXT_LIMIT, omp::parse_omp,
};

/// The supported agent harnesses (strict enum — no free strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    /// The `oh-my-pi` (`omp`) harness.
    Omp,
    /// Anthropic's Claude Code CLI.
    ClaudeCode,
    /// `Codex` CLI.
    Codex,
}

impl AgentKind {
    /// All supported agent kinds, in canonical order.
    pub const ALL: [Self; 3] = [Self::Omp, Self::ClaudeCode, Self::Codex];

    /// The canonical identifier used in session paths and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Parses a kind from a string, case-insensitively.
    ///
    /// In addition to the canonical identifiers, `claude` and `claude_code`
    /// are accepted as aliases for [`AgentKind::ClaudeCode`].
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
        None
    }

    /// A human-friendly display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Omp => "Omp",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
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
pub fn read_session(kind: AgentKind, path: &Path) -> IoResult<Vec<SessionMessage>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(match kind {
        AgentKind::Omp => parse_omp(&raw),
        AgentKind::ClaudeCode => parse_claude_code(&raw),
        AgentKind::Codex => parse_codex(&raw),
    })
}

/// Reads the session's own title from its transcript, when the harness
/// records one. Stable — unlike herdr's terminal title, which animates —
/// so the post title only changes when the task does.
///
/// - `omp`: `{"type":"title","title":…}` (new) /
///   `{"type":"title_change","title":…}` (legacy) records; the last one wins.
/// - `claude-code`: `custom-title` (user-set), `ai-title` (auto), or the
///   first-line `summary`, in that priority.
/// - `codex`: no title record; `None`.
///
/// `None` when the file is missing or no usable title exists yet.
#[must_use]
pub fn read_session_title(kind: AgentKind, path: &Path) -> Option<String> {
    if kind == AgentKind::Codex {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut title: Option<String> = None;
    let mut custom: Option<String> = None;
    let mut ai: Option<String> = None;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match (kind, value.get("type").and_then(Value::as_str)) {
            (AgentKind::Omp, Some("title" | "title_change")) => {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (AgentKind::ClaudeCode, Some("custom-title")) => {
                custom = value
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (AgentKind::ClaudeCode, Some("ai-title")) => {
                ai = value
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            (AgentKind::ClaudeCode, Some("summary")) => {
                title = value
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    let chosen = match kind {
        AgentKind::ClaudeCode => custom.or(ai).or(title),
        _ => title,
    };
    chosen
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        AgentKind, SessionRole, TOOL_TEXT_LIMIT, ToolCallId, ToolState, claude::parse_claude_code,
        codex::parse_codex, omp::parse_omp, read_session, read_session_title,
    };

    #[test]
    fn agent_kind_parse_round_trips() {
        for kind in AgentKind::ALL {
            assert_eq!(AgentKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(AgentKind::Omp.as_str(), "omp");
        assert_eq!(AgentKind::ClaudeCode.as_str(), "claude-code");
        assert_eq!(AgentKind::Codex.as_str(), "codex");
        assert_eq!(AgentKind::Omp.label(), "Omp");
        assert_eq!(AgentKind::ClaudeCode.label(), "Claude Code");
        assert_eq!(AgentKind::Codex.label(), "Codex");
        assert_eq!(AgentKind::ALL.len(), 3);

        assert_eq!(AgentKind::parse("OMP"), Some(AgentKind::Omp));
        assert_eq!(AgentKind::parse("OmP"), Some(AgentKind::Omp));
        assert_eq!(AgentKind::parse("CLAUDE-CODE"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("claude_code"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("Claude_Code"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::parse("CODEX"), Some(AgentKind::Codex));

        assert_eq!(AgentKind::parse(""), None);
        assert_eq!(AgentKind::parse("gpt"), None);
        assert_eq!(AgentKind::parse(" claude-code "), None);
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
            read_session_title(AgentKind::Omp, &path).as_deref(),
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
            read_session_title(AgentKind::ClaudeCode, &path).as_deref(),
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
            read_session_title(AgentKind::ClaudeCode, &path).as_deref(),
            Some("Auto title")
        );
        std::fs::write(&path, r#"{"type":"summary","summary":"A summary title"}"#).unwrap();
        assert_eq!(
            read_session_title(AgentKind::ClaudeCode, &path).as_deref(),
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
        assert_eq!(read_session_title(AgentKind::Codex, &path), None);
        std::fs::remove_file(&path).ok();
        let missing =
            std::env::temp_dir().join(format!("herdcord-title-missing-{}", std::process::id()));
        assert_eq!(read_session_title(AgentKind::Omp, &missing), None);
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
        let messages = read_session(AgentKind::Omp, &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hello");

        let missing = std::env::temp_dir().join("herdcord-session-does-not-exist.jsonl");
        assert!(read_session(AgentKind::Omp, &missing).is_err());
    }
}

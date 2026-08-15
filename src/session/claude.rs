//! The Claude Code harness: transcript parsing, titles, and resume
//! arguments.

use std::{io::Result as IoResult, path::Path};

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{
        AGENT_TEXT_TYPES, compact_args, content_text, read_transcript, scan_completions,
        tool_message, transcript_title,
    },
};

/// The Claude Code harness. Sessions are JSONL transcript files; the type
/// owns their parsing, their title records, and the resume arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeCode;

/// The completion records in a claude `tool_result` line, if any.
fn claude_completions(value: &Value) -> Vec<(ToolCallId, bool, String)> {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let message = value.get("message");
    let content = message
        .and_then(|m| m.get("content"))
        .or_else(|| value.get("content"));
    let Some(Value::Array(blocks)) = content else {
        return Vec::new();
    };
    let mut completions = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = content_text(block.get("content"), &AGENT_TEXT_TYPES).unwrap_or_default();
        completions.push((ToolCallId::from(tool_use_id), is_error, text));
    }
    completions
}

impl ClaudeCode {
    /// Parses a Claude Code session transcript.
    #[must_use]
    pub fn parse_transcript(raw: &str) -> Vec<SessionMessage> {
        // Pre-scan completion records: tool results arrive as `tool_result`
        // content blocks on `user` lines, and the file is parsed whole, so
        // every call's state is known up front.
        let results = scan_completions(raw, claude_completions);

        let mut messages = Vec::new();
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let role = match value.get("type").and_then(Value::as_str) {
                Some("user") => SessionRole::User,
                Some("assistant") => SessionRole::Agent,
                Some("message") => match value.pointer("/message/role").and_then(Value::as_str) {
                    Some("user") => SessionRole::User,
                    Some("assistant") => SessionRole::Agent,
                    _ => continue,
                },
                // Summary, file-history-snapshot, tool_result and other metadata
                // lines never carry conversation.
                _ => continue,
            };
            let message = value.get("message");
            // Some Claude Code lines put the content at the top level.
            let content = message
                .and_then(|m| m.get("content"))
                .or_else(|| value.get("content"));

            // Tool calls are recorded as `tool_use` content blocks: one message
            // per call, after the assistant text.
            let mut tools = Vec::new();
            if let Some(Value::Array(blocks)) = content {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let Some(name) = block.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToolCallId::from)
                        .unwrap_or_default();
                    let args = block.get("input").map(compact_args);
                    tools.push(tool_message(name.to_owned(), call_id, args, &results));
                }
            }

            let Some(text) = content_text(content, &AGENT_TEXT_TYPES) else {
                messages.extend(tools);
                continue;
            };
            if text.trim().is_empty() {
                messages.extend(tools);
                continue;
            }
            messages.push(SessionMessage {
                role,
                text,
                tool: None,
            });
            messages.extend(tools);
        }
        messages
    }

    /// Reads and parses a Claude Code transcript file.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::NotFound`] when the file is missing.
    pub fn read_session(path: &Path) -> IoResult<Vec<SessionMessage>> {
        read_transcript(path, Self::parse_transcript)
    }

    /// The session's own title from its transcript, when recorded:
    /// `custom-title` (user-set), `ai-title` (auto), or the first-line
    /// `summary`, in that priority.
    #[must_use]
    pub fn read_title(path: &Path) -> Option<String> {
        let custom = transcript_title(path, &["custom-title"], "customTitle");
        custom
            .or_else(|| transcript_title(path, &["ai-title"], "aiTitle"))
            .or_else(|| transcript_title(path, &["summary"], "summary"))
    }

    /// The `agent.start` arguments that resume a session: Claude Code
    /// resumes by session id.
    #[must_use]
    pub fn resume_args(session: &str) -> Vec<String> {
        vec!["--resume".into(), session.to_owned()]
    }
}

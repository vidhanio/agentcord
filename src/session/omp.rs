//! The `omp` harness: transcript parsing, titles, and resume arguments.

use std::{collections::VecDeque, io::Result as IoResult, path::Path};

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{
        compact_args, content_text, read_transcript, scan_completions, tool_message,
        transcript_title,
    },
};

/// The `omp` harness (`oh-my-pi`). Sessions are JSONL transcript files;
/// the type owns their parsing, their title records, and the resume
/// arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Omp;

/// Text-bearing content block types for `omp` transcripts.
const OMP_TEXT_TYPES: [&str; 1] = ["text"];
/// The completion record in an `omp` tool-result line, if any.
fn omp_completion(value: &Value) -> Vec<(ToolCallId, bool, String)> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return Vec::new();
    }
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    if message.get("role").and_then(Value::as_str) != Some("toolResult") {
        return Vec::new();
    }
    let Some(call_id) = message.get("toolCallId").and_then(Value::as_str) else {
        return Vec::new();
    };
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = content_text(message.get("content"), &OMP_TEXT_TYPES).unwrap_or_default();
    vec![(ToolCallId::from(call_id), is_error, text)]
}

impl Omp {
    /// Parses an `omp` session transcript.
    ///
    /// # Panics
    ///
    /// Panics if a truncated `tool_execution_start` record's argument
    /// replacement races the pre-scan bookkeeping — the position check
    /// immediately precedes the removal, so this cannot happen.
    #[must_use]
    pub fn parse_transcript(raw: &str) -> Vec<SessionMessage> {
        // Pre-scan completion records: tool results arrive as `message` lines
        // with role `toolResult`, and the file is parsed whole, so every call's
        // state is known up front.
        let results = scan_completions(raw, omp_completion);

        // Pre-scan the full arguments from the assistant `toolCall` message
        // records: omp truncates the args it records in `tool_execution_start`
        // (about 230 characters), so a truncated record falls back to the
        // message's full arguments, matched by tool name in record order.
        let mut full_args: VecDeque<(String, String)> = VecDeque::new();
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = value.get("message") else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            for block in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                let (Some(name), Some(arguments)) = (
                    block.get("name").and_then(Value::as_str),
                    block.get("arguments"),
                ) else {
                    continue;
                };
                full_args.push_back((name.to_owned(), compact_args(arguments)));
            }
        }

        let mut messages = Vec::new();
        for line in raw.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            // Tool calls are recorded as `custom` lines; emit one message per call.
            if value.get("type").and_then(Value::as_str) == Some("custom")
                && value.get("customType").and_then(Value::as_str) == Some("tool_execution_start")
            {
                let Some(data) = value.get("data") else {
                    continue;
                };
                let Some(name) = data.get("toolName").and_then(Value::as_str) else {
                    continue;
                };
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(ToolCallId::from)
                    .unwrap_or_default();
                // Some tools (`hub`, `task`, …) are recorded without arguments;
                // the record's intent summary is their only input, so it stands
                // in as the single argument.
                let args = data.get("args").map(compact_args).or_else(|| {
                    data.get("intent")
                        .and_then(Value::as_str)
                        .map(|intent| compact_args(&Value::String(intent.to_owned())))
                });
                // A record whose args end in an ellipsis was truncated by omp;
                // the message record carries the full arguments.
                let args = args.map(|args| {
                    if !args.trim_end_matches(['}', '"']).ends_with('…') {
                        return args;
                    }
                    full_args
                        .iter()
                        .position(|(full_name, _)| full_name == name)
                        .map_or(args, |index| {
                            full_args.remove(index).expect("position checked").1
                        })
                });
                messages.push(tool_message(name.to_owned(), call_id, args, &results));
                continue;
            }
            if value.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = value.get("message") else {
                continue;
            };
            let role = match message.get("role").and_then(Value::as_str) {
                Some("user") => SessionRole::User,
                Some("assistant") => SessionRole::Agent,
                // Tool results and any other non-conversation roles never carry
                // conversation text: only user/assistant roles produce messages.
                _ => continue,
            };
            let Some(text) = content_text(message.get("content"), &OMP_TEXT_TYPES) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            messages.push(SessionMessage {
                role,
                text,
                tool: None,
            });
        }
        messages
    }

    /// Reads and parses an `omp` transcript file.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::NotFound`] when the file is missing.
    pub fn read_session(path: &Path) -> IoResult<Vec<SessionMessage>> {
        read_transcript(path, Self::parse_transcript)
    }

    /// The session's own title from its transcript, when recorded:
    /// `{"type":"title"|"title_change","title":…}` records; the last one
    /// wins.
    #[must_use]
    pub fn read_title(path: &Path) -> Option<String> {
        transcript_title(path, &["title", "title_change"], "title")
    }

    /// The `agent.start` arguments that resume a session: omp resumes by
    /// transcript path.
    #[must_use]
    pub fn resume_args(transcript: &str) -> Vec<String> {
        vec![format!("--resume={transcript}")]
    }
}

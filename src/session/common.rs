//! Shared transcript-parsing helpers: text extraction, caps, the
//! completion pre-scan, and the tool-call message builder.

use std::collections::HashMap;

use serde_json::Value;

use super::{SessionMessage, SessionRole, ToolCall, ToolCallId, ToolState};

/// Text-bearing content block types for Claude Code and Codex transcripts.
pub const AGENT_TEXT_TYPES: [&str; 3] = ["text", "input_text", "output_text"];

/// Tool-call text is capped: arguments can embed entire file contents, so
/// only this many characters are kept, with an ellipsis appended.
pub const TOOL_TEXT_LIMIT: usize = 400;

/// Cuts `text` to `limit` characters, appending `…` when truncated
/// (char-safe).
#[must_use]
pub fn cap(text: &str, limit: usize) -> String {
    let mut truncated: String = text.chars().take(limit).collect();
    if truncated.chars().count() == text.chars().count() {
        text.to_owned()
    } else {
        truncated.push('…');
        truncated
    }
}

/// Compact-JSON serialization of a call's arguments (not truncated: the
/// embed splits it into per-field blocks, and display caps it).
#[must_use]
pub fn compact_args(args: &Value) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| args.to_string())
}

/// Extracts the conversation text from a `content` value.
///
/// A plain string is used verbatim; an array contributes the `text` fields of
/// the blocks whose `type` is in `text_types`, joined with `\n`. All other
/// block types (tool results, thinking, and so on) are skipped. Returns
/// `None` when the value carries no text at all.
#[must_use]
pub fn content_text(content: Option<&Value>, text_types: &[&str]) -> Option<String> {
    match content {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(parts)) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| {
                    let ty = part.get("type").and_then(Value::as_str)?;
                    if !text_types.contains(&ty) {
                        return None;
                    }
                    part.get("text").and_then(Value::as_str)
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

/// The state and error text for `call_id` from the completion pre-scan:
/// failed when the completion reports an error, done when it completed
/// cleanly, running while no completion is present.
pub fn completion_state(
    results: &HashMap<ToolCallId, (bool, String)>,
    call_id: &ToolCallId,
) -> (ToolState, Option<String>) {
    match results.get(call_id) {
        Some((true, text)) => (ToolState::Failed, Some(cap(text, TOOL_TEXT_LIMIT))),
        Some((false, _)) => (ToolState::Done, None),
        None => (ToolState::Running, None),
    }
}

/// Pre-scans a transcript for tool completion records: call id →
/// (errored, raw output text). `extract` pulls every completion record out
/// of one JSON line; each harness's record shape differs (omp `toolResult`
/// messages, claude `tool_result` blocks, codex `*_call_output` items), so
/// only that navigation is per-harness.
pub fn scan_completions(
    raw: &str,
    extract: impl Fn(&Value) -> Vec<(ToolCallId, bool, String)>,
) -> HashMap<ToolCallId, (bool, String)> {
    let mut results: HashMap<ToolCallId, (bool, String)> = HashMap::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for (call_id, is_error, text) in extract(&value) {
            results.insert(call_id, (is_error, text));
        }
    }
    results
}

/// Builds a tool-call message: the call itself (with its completion-derived
/// state) and the display text.
#[must_use]
pub fn tool_message(
    name: String,
    call_id: ToolCallId,
    args: Option<String>,
    results: &HashMap<ToolCallId, (bool, String)>,
) -> SessionMessage {
    let (state, error) = completion_state(results, &call_id);
    let call = ToolCall {
        call_id,
        name,
        args,
        state,
        error,
    };
    let text = call.to_string();
    SessionMessage {
        role: SessionRole::Tool,
        text,
        tool: Some(call),
    }
}

//! The `omp` transcript parser.

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{compact_args, content_text, scan_completions, tool_message},
};

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

/// Parses an `omp` session transcript.
#[must_use]
pub fn parse_omp(raw: &str) -> Vec<SessionMessage> {
    // Pre-scan completion records: tool results arrive as `message` lines
    // with role `toolResult`, and the file is parsed whole, so every call's
    // state is known up front.
    let results = scan_completions(raw, omp_completion);

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
            let args = data.get("args").map(compact_args);
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

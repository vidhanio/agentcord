//! The Codex transcript parser.

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{AGENT_TEXT_TYPES, compact_args, content_text, scan_completions, tool_message},
};
/// The completion record in a codex `*_call_output` line, if any.
fn codex_completions(value: &Value) -> Vec<(ToolCallId, bool, String)> {
    let Some(payload) = value.get("payload") else {
        return Vec::new();
    };
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    ) {
        return Vec::new();
    }
    let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let is_error = payload
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = match payload.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    vec![(ToolCallId::from(call_id), is_error, text)]
}

/// Parses a Codex session transcript.
#[must_use]
pub fn parse_codex(raw: &str) -> Vec<SessionMessage> {
    // Tool results arrive as `function_call_output` / `custom_tool_call_output`
    // items, and the file is parsed whole, so every call's state is known
    // up front.
    let results = scan_completions(raw, codex_completions);

    let mut messages = Vec::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let (role, content) = match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                let payload_type = payload.get("type").and_then(Value::as_str);
                // Tool calls are recorded as `function_call` / `custom_tool_call`
                // items: one message per call, with the arguments re-parsed
                // from their JSON string form.
                if matches!(payload_type, Some("function_call" | "custom_tool_call")) {
                    let Some(name) = payload.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let call_id = payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(ToolCallId::from)
                        .unwrap_or_default();
                    let args = payload.get("arguments").map(|arguments| match arguments {
                        Value::String(raw) => serde_json::from_str::<Value>(raw)
                            .unwrap_or_else(|_| Value::String(raw.clone())),
                        other => other.clone(),
                    });
                    let args = args.as_ref().map(compact_args);
                    messages.push(tool_message(name.to_owned(), call_id, args, &results));
                    continue;
                }
                if payload_type != Some("message") {
                    continue;
                }
                let role = match payload.get("role").and_then(Value::as_str) {
                    Some("user") => SessionRole::User,
                    // Unknown roles fall back to the agent side.
                    _ => SessionRole::Agent,
                };
                (role, payload.get("content"))
            }
            Some("event_msg") => {
                if payload.get("type").and_then(Value::as_str) != Some("user_message") {
                    continue;
                }
                (SessionRole::User, payload.get("message"))
            }
            // session_meta, function_call_output, custom_tool_call_output,
            // reasoning, token_count, turn_context and any other line types
            // never carry conversation.
            _ => continue,
        };
        let Some(text) = content_text(content, &AGENT_TEXT_TYPES) else {
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

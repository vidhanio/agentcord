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

#[cfg(test)]
mod tests {
    use super::parse_codex;
    use crate::session::{SessionRole, ToolCallId, ToolState};

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
}

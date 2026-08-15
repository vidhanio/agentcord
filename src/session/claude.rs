//! The Claude Code transcript parser.

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{AGENT_TEXT_TYPES, compact_args, content_text, scan_completions, tool_message},
};
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

/// Parses a Claude Code session transcript.
#[must_use]
pub fn parse_claude_code(raw: &str) -> Vec<SessionMessage> {
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

#[cfg(test)]
mod tests {
    use super::parse_claude_code;
    use crate::session::{SessionRole, ToolCallId, ToolState};

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
}

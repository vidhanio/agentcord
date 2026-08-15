//! The `omp` transcript parser.

use std::collections::VecDeque;

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

#[cfg(test)]
mod tests {
    use super::parse_omp;
    use crate::session::{SessionRole, ToolCallId, ToolState, common::TOOL_TEXT_LIMIT};

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
}

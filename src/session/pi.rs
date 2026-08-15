//! The `pi` transcript parser.
//!
//! Pi writes JSONL sessions under `~/.pi/agent/sessions/<encoded-cwd>/`.
//! Tool calls are `toolCall` content blocks inside `assistant` messages;
//! their results arrive as sibling `toolResult` messages (or, for `!`
//! commands, a single combined `bashExecution` message). Metadata entries
//! (`session`, `model_change`, `thinking_level_change`, `session_info`, …)
//! never carry conversation.

use std::fmt::Write;

use serde_json::Value;

use super::{
    SessionMessage, SessionRole, ToolCallId,
    common::{compact_args, content_text, scan_completions, tool_message},
};

/// Text-bearing content block types for `pi` transcripts. `thinking` blocks
/// are deliberately excluded: they are not conversation.
const PI_TEXT_TYPES: [&str; 1] = ["text"];

/// The completion records in a pi `toolResult` or `bashExecution` line, if any.
fn pi_completions(value: &Value) -> Vec<(ToolCallId, bool, String)> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return Vec::new();
    }
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    match message.get("role").and_then(Value::as_str) {
        // Tool results arrive as `toolResult` messages holding the result of
        // one `toolCall` content block.
        Some("toolResult") => {
            let Some(call_id) = message.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let is_error = message
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = content_text(message.get("content"), &PI_TEXT_TYPES).unwrap_or_default();
            vec![(ToolCallId::from(call_id), is_error, text)]
        }
        // `!`-commands are recorded as `bashExecution` messages carrying the
        // call and its result in one record; the entry id pairs them.
        Some("bashExecution") => {
            let Some(call_id) = value.get("id").and_then(Value::as_str) else {
                return Vec::new();
            };
            let is_error = message
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
            let text = message
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            vec![(ToolCallId::from(call_id), is_error, text)]
        }
        _ => Vec::new(),
    }
}

/// Condenses inlined `<skill name="…" …>…</skill>` blocks to a short
/// `/skill:name` marker. Pi inlines a skill's whole `SKILL.md` into the
/// user turn that invoked it; without this the Discord mirror would echo
/// hundreds of lines of context per invocation (and can exceed Discord's
/// message limit). Nested blocks are matched with a depth counter; a block
/// with no closing tag is left verbatim (transcripts may be truncated).
#[must_use]
fn condense_skills(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = find_skill_open(rest) {
        out.push_str(&rest[..open]);
        let tag = &rest[open..];
        let open_end = tag.find('>').expect("`<skill` open tag ends with `>`");
        let name = skill_name(&tag[..=open_end]);
        let content = &tag[open_end + 1..];
        if let Some(close) = find_skill_close(content) {
            let _ = write!(out, "/skill:{name}");
            rest = &content[close..];
        } else {
            out.push_str(tag);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// The byte offset of the next `<skill` opening tag (with a space or `>`
/// after the name), or `None`.
fn find_skill_open(text: &str) -> Option<usize> {
    text.match_indices("<skill").find_map(|(i, _)| {
        (text
            .as_bytes()
            .get(i + 6)
            .is_some_and(|b| matches!(b, b' ' | b'>')))
        .then_some(i)
    })
}

/// The byte offset just past the `</skill>` closing the block that starts
/// after the opening tag, counting nested `<skill>` opens; `None` when the
/// block is unclosed.
fn find_skill_close(after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = 0usize;
    let bytes = after_open.as_bytes();
    while i < bytes.len() {
        if bytes[i..].starts_with(b"</skill>") {
            depth -= 1;
            if depth == 0 {
                return Some(i + 8);
            }
            i += 8;
        } else if bytes[i..].starts_with(b"<skill")
            && bytes.get(i + 6).is_some_and(|b| matches!(b, b' ' | b'>'))
        {
            depth += 1;
            i += 6;
        } else {
            i += 1;
        }
    }
    None
}

/// The `name` attribute of a `<skill …>` opening tag, or `skill` when it
/// is missing or malformed.
fn skill_name(open_tag: &str) -> &str {
    open_tag
        .find("name=\"")
        .and_then(|start| {
            let rest = &open_tag[start + "name=\"".len()..];
            rest.find('"').map(|end| &rest[..end])
        })
        .filter(|name| !name.is_empty())
        .unwrap_or("skill")
}

/// Parses a `pi` session transcript.
#[must_use]
pub fn parse_pi(raw: &str) -> Vec<SessionMessage> {
    // Pre-scan completion records: tool results arrive as `toolResult` and
    // `bashExecution` message lines, and the file is parsed whole, so every
    // call's state is known up front.
    let results = scan_completions(raw, pi_completions);

    let mut messages = Vec::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            // Session headers, model/thinking-level changes, `session_info`
            // records, compaction/branch summaries, extension records and
            // labels never carry conversation.
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                let Some(text) = content_text(message.get("content"), &PI_TEXT_TYPES) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                messages.push(SessionMessage {
                    role: SessionRole::User,
                    // Pi inlines invoked skills into the user turn; the
                    // mirror shows the invocation, not the skill's whole
                    // `SKILL.md`.
                    text: condense_skills(&text),
                    tool: None,
                });
            }
            Some("assistant") => {
                // Tool calls are `toolCall` content blocks: one message per
                // call, after the assistant text.
                let mut tools = Vec::new();
                if let Some(Value::Array(blocks)) = message.get("content") {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
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
                        let args = block.get("arguments").map(compact_args);
                        tools.push(tool_message(name.to_owned(), call_id, args, &results));
                    }
                }
                let Some(text) = content_text(message.get("content"), &PI_TEXT_TYPES) else {
                    messages.extend(tools);
                    continue;
                };
                if text.trim().is_empty() {
                    messages.extend(tools);
                    continue;
                }
                messages.push(SessionMessage {
                    role: SessionRole::Agent,
                    text,
                    tool: None,
                });
                messages.extend(tools);
            }
            // `!`-commands are recorded as a single `bashExecution` message
            // carrying the call and its result: emit the call, anchored to
            // the completion pre-scan.
            Some("bashExecution") => {
                let Some(command) = message.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let call_id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToolCallId::from)
                    .unwrap_or_default();
                // The record's fields are the call's arguments: the single
                // `command` field flows through the same single-argument
                // rendering path as any other tool call.
                let args = compact_args(&serde_json::json!({ "command": command }));
                messages.push(tool_message(
                    "bash".to_owned(),
                    call_id,
                    Some(args),
                    &results,
                ));
            }
            // Tool results and any other non-conversation roles (`custom`
            // extension messages, branch/compaction summaries, …) never
            // carry conversation text.
            _ => {}
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::{
        super::{SessionRole, TOOL_TEXT_LIMIT, ToolCallId, ToolState},
        condense_skills, parse_pi,
    };

    #[test]
    fn pi_messages_parsed() {
        let raw = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-08-12T09:50:11.846Z","cwd":"/home/vidhanio/Projects/herdcord"}
{"type":"model_change","id":"m","parentId":null,"timestamp":"2026-08-12T09:50:11.916Z","provider":"opencode-go","modelId":"deepseek-v4-flash"}
{"type":"thinking_level_change","id":"t","parentId":"m","timestamp":"2026-08-12T09:50:11.916Z","thinkingLevel":"max"}
{"type":"message","id":"u1","parentId":"t","timestamp":"2026-08-12T09:51:55.348Z","message":{"role":"user","content":[{"type":"text","text":"create a discord bot"}]}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-12T09:51:58.780Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me start"},{"type":"text","text":"I will inspect the files."}]}}
{"type":"session_info","id":"i1","parentId":"a1","timestamp":"2026-08-12T09:52:00.000Z","name":"a session title"}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "create a discord bot");
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "I will inspect the files.");
    }

    #[test]
    fn pi_user_skills_condensed() {
        let raw = r#"{"type":"message","id":"u1","parentId":"t","timestamp":"2026-08-15T13:12:42.782Z","message":{"role":"user","content":[{"type":"text","text":"<skill name=\"setup-matt-pocock-skills\" location=\"/home/vidhanio/.agents/skills/…/SKILL.md\">\n# The skill\n</skill>\n\nrewrite my agents.md"}]}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(
            messages[0].text,
            "/skill:setup-matt-pocock-skills\n\nrewrite my agents.md"
        );
    }

    #[test]
    fn condense_skills_nested_blocks() {
        let text = "<skill name=\"outer\">\n<skill name=\"inner\">x</skill>\ny\n</skill>\nrest";
        assert_eq!(condense_skills(text), "/skill:outer\nrest");
    }

    #[test]
    fn condense_skills_leaves_unclosed_block() {
        let text = "<skill name=\"x\">\ncontent";
        assert_eq!(condense_skills(text), text);
    }

    #[test]
    fn condense_skills_passthrough_without_skill_tags() {
        let text = "plain <skillname> not a skill</skillname> text";
        assert_eq!(condense_skills(text), text);
    }

    #[test]
    fn condense_skills_defaults_missing_name() {
        let text = "<skill>\ncontent</skill>\nrest";
        assert_eq!(condense_skills(text), "/skill:skill\nrest");
    }

    #[test]
    fn pi_tool_calls_parsed() {
        let raw = r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-12T09:54:00.240Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me read the file"},{"type":"toolCall","id":"call_00_ET_eJKNhqweSfy1LrJfuvtC8152","name":"read","arguments":{"path":"src/session"}}]}}
{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-12T09:54:00.468Z","message":{"role":"toolResult","toolCallId":"call_00_ET_eJKNhqweSfy1LrJfuvtC8152","toolName":"read","content":[{"type":"text","text":"file contents"}],"isError":false}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, SessionRole::Tool);
        assert_eq!(messages[0].text, r#"read {"path":"src/session"}"#);
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(
            call.call_id,
            ToolCallId::from("call_00_ET_eJKNhqweSfy1LrJfuvtC8152")
        );
        assert_eq!(call.name, "read");
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src/session"}"#));
        // The transcript carries a clean `toolResult` for this call.
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
    }

    #[test]
    fn pi_tool_calls_anchor_to_results() {
        let blob = "x".repeat(500);
        let raw = format!(
            r#"{{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-12T09:54:00.240Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call_0","name":"read","arguments":{{"path":"src"}}}}]}}}}
{{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-08-12T09:54:01.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call_1","name":"write","arguments":{{"path":"out.txt"}}}}]}}}}
{{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-08-12T09:54:02.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"call_2","name":"ask","arguments":{{"question":"ok?"}}}}]}}}}
{{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-12T09:54:03.000Z","message":{{"role":"toolResult","toolCallId":"call_0","toolName":"read","content":[{{"type":"text","text":"ok"}}]}}}}
{{"type":"message","id":"r2","parentId":"a2","timestamp":"2026-08-12T09:54:04.000Z","message":{{"role":"toolResult","toolCallId":"call_1","toolName":"write","isError":true,"content":[{{"type":"text","text":"{blob}"}}]}}}}"#
        );
        let messages = parse_pi(&raw);
        assert_eq!(messages.len(), 3);
        // Done: clean toolResult present.
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_0"));
        assert_eq!(call.args.as_deref(), Some(r#"{"path":"src"}"#));
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        assert_eq!(messages[0].text, r#"read {"path":"src"}"#);
        // Failed: toolResult with isError, error capped at TOOL_TEXT_LIMIT.
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_1"));
        assert_eq!(call.state, ToolState::Failed);
        let error = call.error.as_deref().unwrap();
        assert_eq!(error.chars().count(), TOOL_TEXT_LIMIT + 1);
        assert!(error.ends_with('…'));
        // Running: no completion record for this call.
        let call = messages[2].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("call_2"));
        assert_eq!(call.state, ToolState::Running);
        assert_eq!(call.error, None);
    }

    #[test]
    fn pi_multiple_tool_calls_in_one_message() {
        let raw = r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-12T09:51:58.780Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"check the repo"},{"type":"toolCall","id":"call_00_185JVHa38Nc7E9ZWUgA45281","name":"bash","arguments":{"command":"git status"}},{"type":"toolCall","id":"call_01_J3lL32avnu92jQomS7ey0970","name":"read","arguments":{"path":"TODO.md"}}]}}
{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-12T09:51:58.817Z","message":{"role":"toolResult","toolCallId":"call_00_185JVHa38Nc7E9ZWUgA45281","toolName":"bash","content":[{"type":"text","text":"On branch main"}]}}
{"type":"message","id":"r2","parentId":"r1","timestamp":"2026-08-12T09:51:58.817Z","message":{"role":"toolResult","toolCallId":"call_01_J3lL32avnu92jQomS7ey0970","toolName":"read","content":[{"type":"text","text":"TODO contents"}]}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::Tool);
        assert_eq!(messages[0].text, r#"bash {"command":"git status"}"#);
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        assert_eq!(messages[1].role, SessionRole::Tool);
        assert_eq!(messages[1].text, r#"read {"path":"TODO.md"}"#);
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
    }

    #[test]
    fn pi_bash_execution_parsed() {
        let raw = r#"{"type":"message","id":"b1","parentId":"u1","timestamp":"2026-08-12T09:55:00.000Z","message":{"role":"bashExecution","command":"ls","output":"src\ntests\n","exitCode":0,"cancelled":false,"truncated":false}}
{"type":"message","id":"b2","parentId":"b1","timestamp":"2026-08-12T09:55:01.000Z","message":{"role":"bashExecution","command":"false","output":"Command exited with code 1","exitCode":1,"cancelled":false,"truncated":false}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::Tool);
        assert_eq!(messages[0].text, r#"bash {"command":"ls"}"#);
        let call = messages[0].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("b1"));
        assert_eq!(call.state, ToolState::Done);
        assert_eq!(call.error, None);
        let call = messages[1].tool.as_ref().unwrap();
        assert_eq!(call.call_id, ToolCallId::from("b2"));
        assert_eq!(call.state, ToolState::Failed);
        assert_eq!(call.error.as_deref(), Some("Command exited with code 1"));
    }

    #[test]
    fn pi_empty_whitespace_dropped() {
        let raw = r#"{"type":"message","id":"m1","timestamp":"2026-08-12T09:55:00.000Z","message":{"role":"user","content":[{"type":"text","text":"   "}]}}
{"type":"message","id":"m2","timestamp":"2026-08-12T09:55:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"only thinking"}]}}
{"type":"message","id":"m3","timestamp":"2026-08-12T09:55:02.000Z","message":{"role":"user","content":"plain string form"}}
{"type":"message","id":"m4","timestamp":"2026-08-12T09:55:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"real reply"}]}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "plain string form");
        assert_eq!(messages[1].role, SessionRole::Agent);
        assert_eq!(messages[1].text, "real reply");
    }

    #[test]
    fn pi_non_message_lines_skipped() {
        let raw = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-08-12T09:50:11.846Z","cwd":"/home/vidhanio/Projects/herdcord"}
{"type":"model_change","id":"m","parentId":null,"timestamp":"2026-08-12T09:50:11.916Z","provider":"opencode-go","modelId":"deepseek-v4-flash"}
{"type":"thinking_level_change","id":"t","parentId":"m","timestamp":"2026-08-12T09:50:11.916Z","thinkingLevel":"max"}
{"type":"session_info","id":"i1","parentId":null,"timestamp":"2026-08-12T09:52:00.000Z","name":"my task"}
{"type":"custom","id":"c1","parentId":"i1","timestamp":"2026-08-12T09:52:01.000Z","customType":"extension-state","data":{"k":"v"}}
{"type":"custom_message","id":"c2","parentId":"c1","timestamp":"2026-08-12T09:52:02.000Z","customType":"note","content":"note text","display":true}
{"type":"compaction","id":"cc1","parentId":"c2","timestamp":"2026-08-12T09:52:03.000Z","summary":"compacted"}
{"type":"label","id":"l1","parentId":"cc1","timestamp":"2026-08-12T09:52:04.000Z","targetId":"a1","label":"bookmark"}
{"type":"message","id":"r1","parentId":"l1","timestamp":"2026-08-12T09:52:05.000Z","message":{"role":"toolResult","toolCallId":"call_0","toolName":"read","content":[{"type":"text","text":"orphan result"}]}}
{"type":"message","id":"m1","parentId":"r1","timestamp":"2026-08-12T09:52:06.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, SessionRole::User);
        assert_eq!(messages[0].text, "hello");
    }

    #[test]
    fn pi_malformed_lines_skipped() {
        let raw = r#"not json at all
{"type":"message","id":"m1","timestamp":"2026-08-12T09:55:00.000Z","message":{"role":"user","content":[{"type":"text","text":"first"}]}}
{ broken json
{"type":"message","id":"m2","timestamp":"2026-08-12T09:55:02.000Z","message":{"role":"assistant","content":"trunc"#;
        let messages = parse_pi(raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "first");
    }
}

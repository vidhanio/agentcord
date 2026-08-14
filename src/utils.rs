use serenity::all::{Colour, CreateEmbed, colours::branding};

use crate::session::{ToolCall, ToolState, cap};

/// The character cap for embed text derived from tool calls: arguments can
/// embed entire file contents, so only this many characters are kept.
const TOOL_EMBED_TEXT_LIMIT: usize = 1000;

/// Discord allows at most 25 fields per embed; any arguments beyond that go
/// into a single overflow field.
const MAX_EMBED_FIELDS: usize = 25;

/// Single-argument values longer than this switch from inline code to a
/// code block; values with newlines switch regardless of length.
const TOOL_TEXT_INLINE_LIMIT: usize = 100;

/// An embed for a tool call: title = the tool name; each top-level JSON
/// field of the arguments becomes its own embed field (the key as bold
/// code, the value as a code block) — the colour carries the state (green
/// when done, red when failed, accent while running). A failed call's
/// error is posted as a reply embed ([`tool_error_embed`]) instead of an
/// embed field.
pub fn tool_embed(call: &ToolCall) -> CreateEmbed {
    let colour = match call.state {
        ToolState::Running => Colour::LIGHT_GREY,
        ToolState::Done => branding::GREEN,
        ToolState::Failed => branding::RED,
    };
    let mut embed = CreateEmbed::new().title(&call.name).colour(colour);
    if let Some(args) = &call.args {
        match serde_json::from_str::<serde_json::Value>(args) {
            Ok(serde_json::Value::Object(fields)) => {
                let mut overflow = 0usize;
                for (index, (key, value)) in fields.into_iter().enumerate() {
                    if index >= MAX_EMBED_FIELDS {
                        overflow += 1;
                        continue;
                    }
                    let body = match &value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    embed = embed.field(
                        key,
                        format!("```\n{}\n```", cap(&body, TOOL_EMBED_TEXT_LIMIT)),
                        false,
                    );
                }
                if overflow > 0 {
                    embed = embed.field(
                        format!("**`… {overflow} more`**"),
                        "```\n…\n```".to_owned(),
                        false,
                    );
                }
            }
            // Not an object (or unparseable): the raw arguments go straight
            // into the embed body.
            _ => {
                embed = embed.description(cap(args, TOOL_EMBED_TEXT_LIMIT));
            }
        }
    }
    embed
}

/// The embed for a failed tool call's error: posted as a reply to the
/// call's own message — title `error`, the error text in a code block.
pub fn tool_error_embed(error: &str) -> CreateEmbed {
    CreateEmbed::new()
        .title("error")
        .description(format!("```\n{}\n```", error.replace("```", "`\u{200b}``")))
        .colour(branding::RED)
}

/// The text form of a tool call with a single argument —
/// ``⚙️ **name** `value` `` while running, ``**name** `value` `` once
/// resolved — or `None` when the call keeps the embed form (no argument,
/// several arguments, or a non-scalar argument). The value is the single
/// field's value with no field name, or a bare scalar string argument
/// (omp's `intent` fallback for argument-less calls); values with
/// newlines or longer than [`TOOL_TEXT_INLINE_LIMIT`] characters switch
/// to a code block. Capped at Discord's message limit rather than an
/// embed field's.
#[must_use]
pub fn tool_call_text(call: &ToolCall) -> Option<String> {
    let args = call.args.as_ref()?;
    let value = match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(fields)) if fields.len() == 1 => {
            fields.into_values().next().expect("exactly one field")
        }
        Ok(value @ serde_json::Value::String(_)) => value,
        _ => return None,
    };
    let body = match &value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let inline = !body.contains('\n') && body.chars().count() <= TOOL_TEXT_INLINE_LIMIT;
    let prefix = if call.state == ToolState::Running {
        "⚙️ "
    } else {
        ""
    };
    let name = &call.name;
    // `**name** ` plus the value delimiters: two backticks inline, the
    // fences and their newlines in a code block.
    let (open, close, delimiters) = if inline {
        ("`", "`", 7)
    } else {
        ("```\n", "\n```", 13)
    };
    // One character of headroom so a truncation ellipsis still fits.
    let budget = serenity::constants::MESSAGE_CODE_LIMIT
        .saturating_sub(prefix.chars().count() + name.chars().count() + delimiters)
        .saturating_sub(1);
    let body = if inline {
        body.replace('`', "`\u{200b}`")
    } else {
        body.replace("```", "`\u{200b}``")
    };
    Some(format!(
        "{prefix}**{name}** {open}{}{close}",
        cap(&body, budget)
    ))
}

/// Splits text into chunks of at most `limit` characters, breaking at line
/// boundaries (overlong lines are hard-split so every chunk fits).
#[must_use]
pub fn split_lines(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for line in text.lines() {
        let mut rest = line;
        while !rest.is_empty() {
            // A separating newline plus at least one character must fit.
            if current_len + usize::from(current_len > 0) >= limit {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            let sep = usize::from(current_len > 0);
            let budget = limit - current_len - sep;
            let line_len = rest.chars().count();
            if sep > 0 {
                current.push('\n');
            }
            if budget >= line_len {
                current.push_str(rest);
                current_len += sep + line_len;
                rest = "";
            } else {
                let head: String = rest.chars().take(budget).collect();
                current_len += sep + budget;
                current.push_str(&head);
                rest = &rest[head.len()..];
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::{split_lines, tool_call_text};
    use crate::session::{ToolCall, ToolCallId, ToolState};

    fn call(name: &str, args: Option<&str>, state: ToolState) -> ToolCall {
        ToolCall {
            call_id: ToolCallId::from("call_0"),
            name: name.to_owned(),
            args: args.map(str::to_owned),
            state,
            error: None,
        }
    }

    #[test]
    fn split_lines_keeps_chunks_under_limit() {
        for chunk in split_lines("one\ntwo\nthree", 5) {
            assert!(chunk.chars().count() <= 5);
        }
    }

    #[test]
    fn split_lines_hard_splits_overlong_lines() {
        let chunks = split_lines(&"x".repeat(2500), 1000);
        assert_eq!(chunks.len(), 3);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 1000);
        }
    }

    #[test]
    fn split_lines_preserves_content() {
        let text = "alpha\nbeta gamma\ndelta";
        let chunks = split_lines(text, 2000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn split_lines_survives_multibyte_chars() {
        let text = "emoji: 🤖🤖🤖\nmore";
        for chunk in split_lines(text, 7) {
            assert!(chunk.chars().count() <= 7);
        }
        assert_eq!(split_lines(text, 7).concat(), text);
    }

    #[test]
    fn single_field_tools_render_as_text() {
        let running = call(
            "read",
            Some(r#"{"path":"src/main.rs"}"#),
            ToolState::Running,
        );
        assert_eq!(
            tool_call_text(&running).as_deref(),
            Some("⚙️ **read** `src/main.rs`")
        );
        let done = call("read", Some(r#"{"path":"src/main.rs"}"#), ToolState::Done);
        assert_eq!(
            tool_call_text(&done).as_deref(),
            Some("**read** `src/main.rs`")
        );
    }

    #[test]
    fn scalar_string_args_render_as_text() {
        let running = call(
            "hub",
            Some(r#""Checking subagent status""#),
            ToolState::Running,
        );
        assert_eq!(
            tool_call_text(&running).as_deref(),
            Some("⚙️ **hub** `Checking subagent status`")
        );
    }

    #[test]
    fn non_single_argument_calls_keep_embeds() {
        // No arguments at all.
        assert_eq!(tool_call_text(&call("ask", None, ToolState::Running)), None);
        // Several fields.
        assert_eq!(
            tool_call_text(&call(
                "edit",
                Some(r#"{"old":"a","new":"b"}"#),
                ToolState::Running
            )),
            None
        );
        // Zero fields.
        assert_eq!(
            tool_call_text(&call("noop", Some("{}"), ToolState::Running)),
            None
        );
        // Non-object, non-scalar JSON.
        assert_eq!(
            tool_call_text(&call("weird", Some("[1,2]"), ToolState::Running)),
            None
        );
        // Unparseable arguments.
        assert_eq!(
            tool_call_text(&call("weird", Some("not json"), ToolState::Running)),
            None
        );
    }

    #[test]
    fn multiline_values_switch_to_code_block() {
        let call = call(
            "write",
            Some(r#"{"content":"a\nb\nc"}"#),
            ToolState::Running,
        );
        assert_eq!(
            tool_call_text(&call).as_deref(),
            Some("⚙️ **write** ```\na\nb\nc\n```")
        );
    }

    #[test]
    fn long_values_switch_to_code_block() {
        let long = "x".repeat(150);
        let call = call(
            "bash",
            Some(&format!(r#"{{"command":"{long}"}}"#)),
            ToolState::Done,
        );
        let text = tool_call_text(&call).unwrap();
        assert!(text.starts_with("**bash** ```\n"));
        assert!(text.ends_with("\n```"));
    }

    #[test]
    fn tool_call_text_capped_at_message_limit() {
        let huge = "y".repeat(5000);
        let call = call(
            "write",
            Some(&format!(r#"{{"content":"{huge}"}}"#)),
            ToolState::Running,
        );
        let text = tool_call_text(&call).unwrap();
        assert!(text.chars().count() <= serenity::constants::MESSAGE_CODE_LIMIT);
        assert!(text.ends_with("…\n```"));
    }

    #[test]
    fn tool_call_text_escapes_backticks() {
        let call = call("read", Some(r#"{"path":"a`b"}"#), ToolState::Done);
        assert_eq!(
            tool_call_text(&call).as_deref(),
            Some("**read** `a`\u{200b}`b`")
        );
    }
}

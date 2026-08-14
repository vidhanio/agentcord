use serenity::all::{Colour, CreateEmbed, colours::branding};

use crate::session::{ToolCall, ToolState, cap};

/// The character cap for embed text derived from tool calls: arguments can
/// embed entire file contents, so only this many characters are kept.
const TOOL_EMBED_TEXT_LIMIT: usize = 1000;

/// Discord allows at most 25 fields per embed; one is reserved for the
/// error field, and any arguments beyond the rest go into a single
/// overflow field.
const MAX_EMBED_FIELDS: usize = 24;

/// An embed for a tool call: title = the tool name; each top-level JSON
/// field of the arguments becomes its own embed field (the key as bold
/// code, the value as a code block) — the colour carries the state (green
/// when done, red when failed, accent while running) — and failed calls
/// get an error field.
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
    if let Some(error) = &call.error {
        embed = embed.field(
            "**`error`**",
            format!("```\n{}\n```", cap(error, TOOL_EMBED_TEXT_LIMIT)),
            false,
        );
    }
    embed
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
    use super::split_lines;

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
}

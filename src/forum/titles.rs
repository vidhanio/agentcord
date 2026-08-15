//! Post titles and the starter message.

use std::path::Path;

use crate::{
    herdr::{Agent, SessionPath},
    session::AgentKind,
};

/// The post title for a session: the transcript's own title when the
/// harness records one (stable — no terminal animation), else herdr's
/// stripped terminal title. Truncated to Discord's 100-character
/// thread-name limit. The agent name is deliberately not used — the
/// transcript/terminal titles describe the work.
#[must_use]
pub fn session_title(agent: &Agent, kind: AgentKind, path: &Path) -> String {
    crate::session::read_session_title(kind, path).map_or_else(
        || post_title(agent, kind),
        |title| title.chars().take(100).collect(),
    )
}

/// The title for a session's forum post: herdr's stripped terminal title
/// when usable, otherwise the kind label plus `" session"`. Truncated to
/// Discord's 100-character thread-name limit. herdr has already removed
/// ANSI escapes and the leading activity glyph, so the title only changes
/// when its text does.
#[must_use]
pub fn post_title(agent: &Agent, kind: AgentKind) -> String {
    let fallback = format!("{} session", kind.as_str());
    let Some(title) = agent.terminal_title_stripped.as_deref() else {
        return fallback;
    };
    let cleaned = title.trim();
    if cleaned.is_empty() {
        return fallback;
    }
    cleaned.chars().take(100).collect()
}

/// Channel-name-sanitizes a workspace label: lowercase `[a-z0-9-]`,
/// truncated to Discord's 100-character channel-name limit, `"agents"` when
/// nothing usable remains. The forum channel is named after the workspace
/// itself.
#[must_use]
pub fn forum_channel_name(label: &str) -> String {
    let mut fragment = String::with_capacity(label.len());
    for ch in label.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            fragment.push(ch);
        } else if !fragment.is_empty() && !fragment.ends_with('-') {
            fragment.push('-');
        }
    }
    while fragment.ends_with('-') {
        fragment.pop();
    }
    if fragment.is_empty() {
        fragment.push_str("agents");
    }
    fragment.chars().take(100).collect()
}

/// The session post's starter message: the pane, cwd, and session file as
/// plain text, plus the checked-out branch when the agent runs in a git
/// worktree. The kind and status are already on the post's tags, so the
/// message stays on one line for the channel preview. For a dead session
/// (no live agent) the pane part is the literal `inactive`.
#[must_use]
pub fn session_intro(
    agent: Option<&Agent>,
    worktree: Option<&str>,
    cwd: &Path,
    session_path: Option<&SessionPath>,
) -> String {
    let pane = agent.map_or_else(
        || "inactive".to_owned(),
        |agent| format!("`{}`", agent.pane_id),
    );
    let worktree = worktree
        .map(|branch| format!(" · worktree `{branch}`"))
        .unwrap_or_default();
    session_path.map_or_else(
        || format!("{pane}{worktree} · cwd `{}`", cwd.display()),
        |path| {
            format!(
                "{pane}{worktree} · cwd `{}` · session `{path}`",
                cwd.display()
            )
        },
    )
}

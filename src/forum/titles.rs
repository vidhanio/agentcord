//! Post titles and the starter message.

use std::path::Path;

use crate::{
    herdr::{Agent, SessionPath},
    session::Harness,
};

/// The post title for a session: the transcript's own title when the
/// harness records one (stable — no terminal animation), else herdr's
/// stripped terminal title. Truncated to Discord's 100-character
/// thread-name limit. The agent name is deliberately not used — the
/// transcript/terminal titles describe the work.
#[must_use]
pub fn session_title(agent: &Agent, harness: Harness, path: &Path) -> String {
    crate::session::read_session_title(harness, path).map_or_else(
        || post_title(agent, harness),
        |title| title.chars().take(100).collect(),
    )
}

/// The title for a session's forum post: herdr's stripped terminal title
/// when usable, otherwise the harness label plus `" session"`. Truncated to
/// Discord's 100-character thread-name limit. herdr has already removed
/// ANSI escapes and the leading activity glyph, so the title only changes
/// when its text does.
#[must_use]
pub fn post_title(agent: &Agent, harness: Harness) -> String {
    let fallback = format!("{} session", harness.as_str());
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
/// worktree. The harness and status are already on the post's tags, so the
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{forum_channel_name, post_title, session_intro};
    use crate::{
        herdr::{Agent, PaneId, SessionPath, TabId, WorkspaceId},
        session::Harness,
    };

    #[test]
    fn forum_channel_name_sanitizes() {
        assert_eq!(forum_channel_name("My Workspace"), "my-workspace");
        assert_eq!(forum_channel_name("UPPER  Case!!"), "upper-case");
        assert_eq!(forum_channel_name("💥"), "agents");
        let long = forum_channel_name(&"a".repeat(200));
        assert_eq!(long.len(), 100);
        assert!(long.ends_with('a'));
    }

    #[test]
    fn post_title_uses_stripped_terminal_title() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("  omp — my project   ".to_owned());
        assert_eq!(post_title(&agent, Harness::Omp), "omp — my project");
    }

    #[test]
    fn post_title_falls_back_to_harness_label() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("   ".to_owned());
        assert_eq!(post_title(&agent, Harness::Omp), "omp session");
        agent.terminal_title_stripped = None;
        assert_eq!(post_title(&agent, Harness::Codex), "codex session");
    }

    #[test]
    fn post_title_truncates() {
        let mut agent = agent_fixture();
        agent.terminal_title_stripped = Some("a".repeat(500));
        assert_eq!(post_title(&agent, Harness::Omp).chars().count(), 100);
    }

    #[test]
    fn session_intro_shows_live_pane() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(
                Some(&agent),
                None,
                Path::new("/home/me"),
                Some(&SessionPath::from("s1"))
            ),
            "`w1:p1` · cwd `/home/me` · session `s1`"
        );
    }

    #[test]
    fn session_intro_marks_inactive_and_skips_missing_session() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(None, None, Path::new("/home/me"), None),
            "inactive · cwd `/home/me`"
        );
        assert_eq!(
            session_intro(Some(&agent), None, Path::new("/home/me"), None),
            "`w1:p1` · cwd `/home/me`"
        );
    }

    #[test]
    fn session_intro_shows_worktree_after_pane() {
        let agent = agent_fixture();
        assert_eq!(
            session_intro(
                Some(&agent),
                Some("feature-x"),
                Path::new("/home/me"),
                Some(&SessionPath::from("s1"))
            ),
            "`w1:p1` · worktree `feature-x` · cwd `/home/me` · session `s1`"
        );
    }

    fn agent_fixture() -> Agent {
        Agent {
            harness: Some(Harness::Omp),
            agent_status: "idle".to_owned(),
            name: Some("agent".to_owned()),
            pane_id: PaneId::from("w1:p1"),
            tab_id: TabId::from("w1:t1"),
            workspace_id: WorkspaceId::from("w1"),
            cwd: PathBuf::from("/home/me"),
            focused: false,
            launch_pending: false,
            terminal_title_stripped: None,
            agent_session: None,
        }
    }
}

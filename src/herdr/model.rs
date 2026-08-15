//! The herdr data model: agent lifecycle status, the nutype ids, and the
//! agent/workspace/tab records reported over the socket.

use std::{path::PathBuf, str::FromStr};

use nutype::nutype;
use serde::Deserialize;

use crate::session::Harness;

/// Lifecycle state of a herdr agent, parsed from its raw status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// The agent is idle and ready for input.
    Idle,
    /// The agent is actively working.
    Working,
    /// The agent is blocked and waiting for input.
    Blocked,
    /// The agent finished its most recent task.
    Done,
    /// The raw status string matched no known state.
    Unknown,
}

impl AgentStatus {
    /// Returns the canonical lowercase status string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

impl FromStr for AgentStatus {
    type Err = ();

    /// Parses a herdr status string case-insensitively; unrecognized strings
    /// return [`Err`], so callers can fall back to [`AgentStatus::Unknown`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "idle" => Ok(Self::Idle),
            "working" => Ok(Self::Working),
            "blocked" => Ok(Self::Blocked),
            "done" => Ok(Self::Done),
            "unknown" => Ok(Self::Unknown),
            _ => Err(()),
        }
    }
}

/// A herdr workspace id, e.g. `"w3"`.
#[nutype(derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Display,
    From,
    Deref,
    Serialize,
    Deserialize
))]
pub struct WorkspaceId(String);

/// A herdr pane id, e.g. `"w4:p2"`.
///
/// Defaults to an empty string when omitted (e.g. `tab.list` rows do not
/// report the root `pane_id`), matching the previous wire behavior.
#[nutype(
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        From,
        Deref,
        Default,
        Serialize,
        Deserialize
    ),
    default = ""
)]
pub struct PaneId(String);

/// A herdr tab id, e.g. `"w4:t2"`.
#[nutype(derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Display,
    From,
    Deref,
    Serialize,
    Deserialize
))]
pub struct TabId(String);

/// An agent session transcript path, e.g. `~/.omp/agent/sessions/…`.
///
/// Defaults to an empty string when herdr omits the field, matching the
/// previous wire behavior.
#[nutype(
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        From,
        Deref,
        Default,
        Serialize,
        Deserialize
    ),
    default = ""
)]
pub struct SessionPath(String);

/// A herdr agent and the pane, tab, and workspace it runs in.
#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    /// The harness the agent runs, e.g. [`Harness::Omp`]; absent while
    /// the agent is still launching or the harness is unknown.
    #[serde(default, rename = "agent", deserialize_with = "de_harness")]
    pub harness: Option<Harness>,
    /// Raw status string as reported by herdr; prefer [`Agent::status`].
    pub agent_status: String,
    /// Unique agent name; absent while the agent is unnamed.
    #[serde(default)]
    pub name: Option<String>,
    /// Id of the pane the agent runs in.
    pub pane_id: PaneId,
    /// Id of the tab the agent runs in.
    pub tab_id: TabId,
    /// Id of the workspace the agent runs in.
    pub workspace_id: WorkspaceId,
    /// Working directory of the agent's pane.
    pub cwd: PathBuf,
    /// Whether the agent's pane is the focused pane.
    #[serde(default)]
    pub focused: bool,
    /// Whether the agent has been detected yet; true for the placeholder
    /// record returned by `agent.start` before startup completes.
    #[serde(default)]
    pub launch_pending: bool,
    /// Terminal title without ANSI escapes, when known. herdr has already
    /// stripped the leading activity glyph (spinner frames); the raw title
    /// is not needed.
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    /// The agent's session reference, when herdr knows one.
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
}

impl Agent {
    /// Parses [`Agent::agent_status`], falling back to [`AgentStatus::Unknown`]
    /// when the status string is unrecognized.
    #[must_use]
    pub fn status(&self) -> AgentStatus {
        self.agent_status.parse().unwrap_or(AgentStatus::Unknown)
    }
}

/// A herdr agent's persistent session reference.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    /// Agent harness, e.g. [`Harness::Omp`]; absent when unknown.
    #[serde(default, rename = "agent", deserialize_with = "de_harness")]
    pub harness: Option<Harness>,
    /// Session kind, e.g. `"jsonl"`.
    #[serde(default)]
    pub kind: String,
    /// Session source, e.g. `"file"`.
    #[serde(default)]
    pub source: String,
    /// Session location, e.g. the transcript file path.
    #[serde(default)]
    pub value: SessionPath,
}

/// Deserializes herdr's optional harness label (`"omp"`, `"claude-code"`,
/// …): unknown labels map to `None` rather than failing the record — an
/// agent of an unknown harness simply gets no session post.
fn de_harness<'de, D>(deserializer: D) -> Result<Option<Harness>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
        .map(|harness| harness.as_deref().and_then(Harness::parse))
}

/// A herdr workspace.
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    /// Unique workspace id.
    pub workspace_id: WorkspaceId,
    /// User-assigned label.
    pub label: String,
    /// Git worktree membership, when the workspace runs in a repo worktree.
    #[serde(default)]
    pub worktree: Option<WorktreeSpace>,
}

/// A herdr workspace's git worktree membership: the repo it belongs to and
/// where its checkout lives.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeSpace {
    /// Canonical git common directory, shared by every workspace of the
    /// repo.
    #[serde(default)]
    pub repo_key: String,
    /// Repo name, e.g. the main checkout's directory name.
    #[serde(default)]
    pub repo_name: String,
    /// The repo's main checkout path.
    #[serde(default)]
    pub repo_root: String,
    /// This workspace's checkout path.
    #[serde(default)]
    pub checkout_path: String,
    /// Whether this is a linked git worktree (a `git worktree add`).
    #[serde(default)]
    pub is_linked_worktree: bool,
}

/// A tab as reported by herdr.
///
/// `tab.create` reports the root pane's `pane_id` and `cwd`, while
/// `tab.list` omits both; `pane_id` therefore defaults to an empty
/// string when absent.
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedTab {
    /// Unique tab id.
    pub tab_id: TabId,
    /// Tab label, when one is set.
    pub label: Option<String>,
    /// Id of the tab's root pane.
    #[serde(default)]
    pub pane_id: PaneId,
    /// Working directory of the root pane, when known.
    pub cwd: Option<String>,
}

/// A workspace created by herdr, together with the id of its root pane.
#[derive(Debug, Clone)]
pub struct CreatedWorkspace {
    /// The created workspace.
    pub workspace: Workspace,
    /// Id of the workspace's root pane.
    pub pane_id: PaneId,
}

/// A point-in-time snapshot of the herdr session: only the agents are
/// consumed (the subscription set is built from them).
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    /// All agents in the session.
    pub agents: Vec<Agent>,
}

#[cfg(test)]
mod tests {
    use super::AgentStatus;

    #[test]
    fn agent_status_round_trips() {
        for (text, expected) in [
            ("idle", AgentStatus::Idle),
            ("working", AgentStatus::Working),
            ("blocked", AgentStatus::Blocked),
            ("done", AgentStatus::Done),
            ("unknown", AgentStatus::Unknown),
        ] {
            assert_eq!(text.parse::<AgentStatus>(), Ok(expected));
            assert_eq!(expected.as_str(), text);
        }
        assert_eq!("IDLE".parse::<AgentStatus>(), Ok(AgentStatus::Idle));
        assert_eq!("Working".parse::<AgentStatus>(), Ok(AgentStatus::Working));
        assert_eq!("bogus".parse::<AgentStatus>(), Err(()));
    }
}

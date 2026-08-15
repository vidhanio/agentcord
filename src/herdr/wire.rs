//! Wire types for the herdr JSON API: the response envelope and the
//! per-method `result` payloads.

use serde::Deserialize;
use serde_json::Value;

use super::{Agent, Error, PaneId, Snapshot, TabId, Workspace};

/// Envelope wrapping every herdr response (success or error).
#[derive(Debug, Deserialize)]
pub struct Envelope {
    /// Successful command payload.
    #[serde(default)]
    pub result: Option<Value>,
    /// Server-reported error, when the request failed.
    #[serde(default)]
    error: Option<ServerError>,
}

impl Envelope {
    /// Converts the envelope into its `result` payload, mapping server errors
    /// to [`Error::Herdr`].
    pub fn into_result(self) -> Result<Value, Error> {
        if let Some(error) = self.error {
            return Err(Error::Herdr {
                code: error.code,
                message: error.message,
            });
        }
        self.result.ok_or_else(|| Error::Herdr {
            code: "missing_result".into(),
            message: "herdr response contained no result".into(),
        })
    }
}

/// Server-reported error payload.
#[derive(Debug, Deserialize)]
struct ServerError {
    /// Machine-readable error code.
    code: String,
    /// Human-readable error message.
    message: String,
}

/// `result` payload of `workspace.list`.
#[derive(Debug, Deserialize)]
pub struct WorkspaceList {
    pub workspaces: Vec<Workspace>,
}

/// `result` payload of `workspace.create`, including the new root pane.
#[derive(Debug, Deserialize)]
pub struct WorkspaceCreated {
    pub workspace: Workspace,
    pub root_pane: RootPane,
}

/// `result` payload of `tab.create`.
#[derive(Debug, Deserialize)]
pub struct TabCreated {
    pub tab: TabRef,
    pub root_pane: RootPane,
}

/// Tab portion of a `tab.create` response.
#[derive(Debug, Deserialize)]
pub struct TabRef {
    pub tab_id: TabId,
    pub label: Option<String>,
}

/// Root pane portion of a `workspace.create`/`tab.create` response.
#[derive(Debug, Deserialize)]
pub struct RootPane {
    pub pane_id: PaneId,
    pub cwd: Option<String>,
}

/// `result` payload of `agent.list`.
#[derive(Debug, Deserialize)]
pub struct AgentList {
    pub agents: Vec<Agent>,
}

/// `result` payload of `agent.get`/`agent.start`/`agent.prompt`/`agent.wait`.
#[derive(Debug, Deserialize)]
pub struct AgentInfo {
    pub agent: Agent,
}

/// `result` payload of `worktree.list`.
#[derive(Debug, Deserialize)]
pub struct WorktreeList {
    /// The repo the listed worktrees belong to; `source_workspace_id` is
    /// the main workspace of the repo, when it is open.
    #[serde(default)]
    pub source: Option<WorktreeSource>,
    #[serde(default)]
    pub worktrees: Vec<WorktreeEntry>,
}

/// The repo a `worktree.list` query scoped to.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeSource {
    /// The main (non-worktree) workspace of the repo, when it is open.
    #[serde(default)]
    pub source_workspace_id: Option<String>,
}

/// One worktree of a repo, as reported by `worktree.list`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeEntry {
    /// The workspace open on this checkout, when it is open.
    #[serde(default)]
    pub open_workspace_id: Option<String>,
    /// The checked-out branch, when not detached.
    #[serde(default)]
    pub branch: Option<String>,
}

/// `result` payload of `session.snapshot`.
#[derive(Debug, Deserialize)]
pub struct SnapshotResult {
    pub snapshot: Snapshot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{herdr::AgentStatus, session::Harness};

    /// Parses the `result` payload of a captured response fixture.
    fn result_of(fixture: &str) -> Value {
        let envelope: Envelope = serde_json::from_str(fixture).unwrap();
        envelope.result.expect("fixture carries a result")
    }
    #[test]
    fn parses_session_snapshot() {
        let result = result_of(include_str!("../../tests/fixtures/api/snapshot.json"));
        assert_eq!(result["type"], "session_snapshot");
        let snapshot: SnapshotResult = serde_json::from_value(result).unwrap();
        assert!(
            snapshot
                .snapshot
                .agents
                .iter()
                .any(|a| a.harness == Some(Harness::Omp) && a.pane_id.as_str() == "w4:p1")
        );
    }

    #[test]
    fn parses_workspace_list() {
        let result = result_of(include_str!("../../tests/fixtures/api/workspace_list.json"));
        let list: WorkspaceList = serde_json::from_value(result).unwrap();
        assert!(list.workspaces.len() >= 2);
        let herdcord = list
            .workspaces
            .iter()
            .find(|w| w.workspace_id.as_str() == "w4")
            .expect("herdcord workspace present");
        assert_eq!(herdcord.label, "herdcord");
        // Regular workspaces are not worktrees.
        assert!(herdcord.worktree.is_none());
    }

    #[test]
    fn parses_worktree_membership() {
        // A worktree workspace carries its repo membership on the wire.
        let value = serde_json::json!({
            "type": "workspace_list",
            "workspaces": [{
                "workspace_id": "w5",
                "number": 3,
                "label": "herdcord",
                "focused": false,
                "pane_count": 1,
                "tab_count": 1,
                "active_tab_id": "w5:t1",
                "agent_status": "idle",
                "worktree": {
                    "repo_key": "/home/vidhanio/Projects/herdcord/.git",
                    "repo_name": "herdcord",
                    "repo_root": "/home/vidhanio/Projects/herdcord",
                    "checkout_path": "/tmp/herdcord-feature",
                    "is_linked_worktree": true
                }
            }]
        });
        let list: WorkspaceList = serde_json::from_value(value).unwrap();
        let worktree = list.workspaces[0]
            .worktree
            .as_ref()
            .expect("worktree membership");
        assert_eq!(worktree.repo_key, "/home/vidhanio/Projects/herdcord/.git");
        assert!(worktree.is_linked_worktree);
    }

    #[test]
    fn parses_worktree_list() {
        let result = result_of(include_str!("../../tests/fixtures/api/worktree_list.json"));
        let list: WorktreeList = serde_json::from_value(result).unwrap();
        let entry = list
            .worktrees
            .iter()
            .find(|entry| entry.open_workspace_id.as_deref() == Some("w4"))
            .expect("main checkout entry present");
        assert_eq!(entry.branch.as_deref(), Some("main"));
    }

    #[test]
    fn parses_agent_list() {
        let result = result_of(include_str!("../../tests/fixtures/api/agent_list.json"));
        let list: AgentList = serde_json::from_value(result).unwrap();
        assert!(!list.agents.is_empty());
        let agent = &list.agents[0];
        assert_eq!(agent.harness, Some(Harness::Omp));
        assert_eq!(agent.workspace_id.as_str(), "w4");
        assert_eq!(agent.pane_id.as_str(), "w4:p1");
        assert_eq!(agent.status(), AgentStatus::Working);
        assert_eq!(agent.name, None);
    }

    #[test]
    fn parses_agent_get() {
        let result = result_of(include_str!("../../tests/fixtures/api/agent_get.json"));
        let info: AgentInfo = serde_json::from_value(result).unwrap();
        let agent = info.agent;
        assert_eq!(agent.harness, Some(Harness::Omp));
        assert_eq!(agent.pane_id.as_str(), "w4:p1");
        // The fixture records an unnamed agent: `name` is either absent (None)
        // or, if herdr later assigns one, present (Some).
        if let Some(name) = &agent.name {
            assert_ne!(name, "");
        }
        assert_eq!(agent.status(), AgentStatus::Working);
    }

    #[test]
    fn parses_agent_get_not_found() {
        let envelope: Envelope = serde_json::from_str(include_str!(
            "../../tests/fixtures/api/agent_get_not_found.json"
        ))
        .unwrap();
        assert!(envelope.result.is_none());
        let error = envelope.error.expect("error envelope present");
        assert_eq!(error.code, "agent_not_found");
        assert!(error.message.contains("definitely-not-real"));
    }

    #[test]
    fn maps_error_envelope_to_herdr_error() {
        let envelope: Envelope = serde_json::from_str(include_str!(
            "../../tests/fixtures/api/agent_get_not_found.json"
        ))
        .unwrap();
        let error = envelope.into_result().unwrap_err();
        match &error {
            Error::Herdr { code, message } => {
                assert_eq!(code, "agent_not_found");
                assert!(message.contains("definitely-not-real"));
            }
            _ => panic!("expected a Herdr error, got {error:?}"),
        }
        assert!(!error.is_timeout());
    }
}

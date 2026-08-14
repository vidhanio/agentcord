//! Typed async client for the herdr Unix-socket JSON API.
//!
//! Each public method maps one herdr API method to a typed result, sends a
//! single newline-delimited JSON request over the herdr Unix socket, and
//! surfaces failures as [`Error`](enum@Error). The event-subscription
//! machinery lives in the `event` submodule; the wire payload types in the
//! `wire` submodule.

use std::{path::PathBuf, str::FromStr, time::Duration};

use nutype::nutype;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::broadcast,
};

mod event;
mod wire;

pub use self::event::{Event, EventKind, EventLine, EventStream, Subscription};
pub(crate) use self::wire::{
    AgentInfo, AgentList, Envelope, SnapshotResult, TabCreated, WorkspaceCreated, WorkspaceList,
    WorktreeList,
};

/// How long `agent.start` waits for the agent to be detected after the
/// placeholder response.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How often `agent.start` polls for detection.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Agent kind label, e.g. `"omp"`; absent while the agent is still
    /// launching.
    #[serde(default)]
    pub agent: Option<String>,
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

/// A herdr agent's persistent session reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// Agent harness name, e.g. `"omp"`.
    #[serde(default)]
    pub agent: String,
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

impl Agent {
    /// Parses [`Agent::agent_status`], falling back to [`AgentStatus::Unknown`]
    /// when the status string is unrecognized.
    #[must_use]
    pub fn status(&self) -> AgentStatus {
        self.agent_status.parse().unwrap_or(AgentStatus::Unknown)
    }
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

/// Typed async client over the herdr Unix-socket API.
#[derive(Debug, Clone)]
pub struct Herdr {
    socket_path: PathBuf,
    operation_timeout: Duration,
}

impl Herdr {
    /// Creates a client that talks to the herdr Unix socket at `socket_path`.
    ///
    /// `operation_timeout` bounds each request's total runtime.
    #[must_use]
    pub const fn new(socket_path: PathBuf, operation_timeout: Duration) -> Self {
        Self {
            socket_path,
            operation_timeout,
        }
    }

    /// Sends one request over a fresh socket connection and returns the
    /// `result` payload.
    ///
    /// herdr answers exactly one request per connection, so each call dials
    /// the socket anew and writes a single newline-delimited JSON line. The
    /// response is a single envelope line.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Timeout`] when the request exceeds `operation_timeout`,
    /// [`Error::Io`] when the socket cannot be reached, [`Error::Json`] when
    /// the response is not a parseable envelope, and [`Error::Herdr`] when the
    /// envelope carries a server error.
    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let request = json!({
            "id": REQUEST_ID,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        let socket_path = self.socket_path.clone();
        let operation = async move {
            let mut stream = UnixStream::connect(&socket_path).await?;
            stream.write_all(line.as_bytes()).await?;
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await?;
            let envelope: Envelope = serde_json::from_slice(&line)?;
            envelope.into_result()
        };
        tokio::time::timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Sends one request and deserializes its `result` payload as `T`.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Herdr::call`], plus [`Error::Json`] when the
    /// result payload does not match `T`.
    async fn call_typed<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, Error> {
        let result = self.call(method, params).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Lists all workspaces (`workspace.list`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>, Error> {
        let list: WorkspaceList = self.call_typed("workspace.list", json!({})).await?;
        Ok(list.workspaces)
    }

    /// Creates a workspace with `label` and root-pane working directory `cwd`
    /// (`workspace.create`, unfocused), returning the workspace together with
    /// the id of its root pane.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn create_workspace_with_pane(
        &self,
        label: &str,
        cwd: &str,
    ) -> Result<CreatedWorkspace, Error> {
        let created: WorkspaceCreated = self
            .call_typed(
                "workspace.create",
                json!({ "label": label, "cwd": cwd, "focus": false }),
            )
            .await?;
        Ok(CreatedWorkspace {
            workspace: created.workspace,
            pane_id: created.root_pane.pane_id,
        })
    }

    /// Closes the workspace with id `id` (`workspace.close`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn close_workspace(&self, id: &WorkspaceId) -> Result<(), Error> {
        let result = self
            .call("workspace.close", json!({ "workspace_id": id }))
            .await?;
        expect_ok(&result, "workspace.close")
    }

    /// Creates a tab in `workspace_id` with `label` and working directory
    /// `cwd` (`tab.create`, unfocused), returning the tab together with the id
    /// and cwd of its root pane.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn create_tab(
        &self,
        workspace_id: &WorkspaceId,
        label: &str,
        cwd: &str,
    ) -> Result<CreatedTab, Error> {
        let created: TabCreated = self
            .call_typed(
                "tab.create",
                json!({
                    "workspace_id": workspace_id,
                    "label": label,
                    "cwd": cwd,
                    "focus": false,
                }),
            )
            .await?;
        Ok(CreatedTab {
            tab_id: created.tab.tab_id,
            label: created.tab.label,
            pane_id: created.root_pane.pane_id,
            cwd: created.root_pane.cwd,
        })
    }

    /// Closes the tab with id `tab_id` (`tab.close`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn close_tab(&self, tab_id: &TabId) -> Result<(), Error> {
        let result = self.call("tab.close", json!({ "tab_id": tab_id })).await?;
        expect_ok(&result, "tab.close")
    }

    /// Lists all agents (`agent.list`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn list_agents(&self) -> Result<Vec<Agent>, Error> {
        let list: AgentList = self.call_typed("agent.list", json!({})).await?;
        Ok(list.agents)
    }

    /// Fetches the agent identified by `target` (a pane id).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`] with code `agent_not_found` when no agent
    /// matches `target`, or [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn get_agent(&self, target: &PaneId) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed("agent.get", json!({ "target": target }))
            .await?;
        Ok(info.agent)
    }

    /// Starts an agent of `kind` in `pane_id` under unique `name`, passing
    /// `agent_args` through to the agent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &PaneId,
        agent_args: &[String],
    ) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed(
                "agent.start",
                json!({
                    "name": name,
                    "kind": kind,
                    "pane_id": pane_id,
                    "args": agent_args,
                }),
            )
            .await?;

        // `agent.start` answers with a placeholder record (launch_pending)
        // before the agent is detected. Poll `agent.get` until the real
        // record appears, like the CLI does.
        let started = std::time::Instant::now();
        loop {
            let agent = match self.get_agent(pane_id).await {
                Ok(agent) => agent,
                Err(Error::Herdr { code, .. }) if code == "agent_not_found" => info.agent.clone(),
                Err(error) => return Err(error),
            };
            if !agent.launch_pending {
                return Ok(agent);
            }
            if started.elapsed() >= STARTUP_TIMEOUT {
                return Err(Error::Herdr {
                    code: "agent_startup_timeout".into(),
                    message: format!("timed out waiting for agent `{name}` to start"),
                });
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Prompts the agent at `target` with `text` and waits for it to settle,
    /// bounded by `timeout`.
    ///
    /// When the agent is still working after `timeout`, herdr answers with a
    /// `timeout` error, which surfaces as an [`Error::Herdr`] whose
    /// [`Error::is_timeout`] returns `true`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn prompt_agent(
        &self,
        target: &PaneId,
        text: &str,
        timeout: Duration,
    ) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed(
                "agent.prompt",
                json!({
                    "target": target,
                    "text": text,
                    "wait": {
                        "until": ["idle", "done", "blocked"],
                        "timeout_ms": timeout_ms(timeout),
                    },
                }),
            )
            .await?;
        Ok(info.agent)
    }

    /// Delivers a prompt to the agent at `target` without waiting for the
    /// turn to settle (`agent.prompt` without `wait`): herdr writes the
    /// text to the agent's input immediately and answers with the agent
    /// record. Settlement is tracked separately with [`Herdr::wait_agent`],
    /// so a long turn never holds the caller.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn send_prompt(&self, target: &PaneId, text: &str) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed(
                "agent.prompt",
                json!({
                    "target": target,
                    "text": text,
                }),
            )
            .await?;
        Ok(info.agent)
    }

    /// Waits for the agent at `target` to settle, bounded by `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`] (with code `timeout` when the agent is still
    /// working), [`Error::Timeout`], [`Error::Io`], or [`Error::Json`] when
    /// the request fails.
    pub async fn wait_agent(&self, target: &PaneId, timeout: Duration) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed(
                "agent.wait",
                json!({
                    "target": target,
                    "until": ["idle", "done", "blocked"],
                    "timeout_ms": timeout_ms(timeout),
                }),
            )
            .await?;
        Ok(info.agent)
    }

    /// Lists the worktrees of `workspace_id`'s repo (`worktree.list`),
    /// including the repo's main workspace and each worktree's
    /// checked-out branch.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub(crate) async fn worktree_list(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<WorktreeList, Error> {
        self.call_typed("worktree.list", json!({ "workspace_id": workspace_id }))
            .await
    }

    /// Takes a point-in-time snapshot of the herdr session
    /// (`session.snapshot`), returning every agent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn session_snapshot(&self) -> Result<Vec<Agent>, Error> {
        let snapshot: SnapshotResult = self.call_typed("session.snapshot", json!({})).await?;
        Ok(snapshot.snapshot.agents)
    }

    /// Stops the herdr server (`server.stop`), terminating its panes.
    ///
    /// The server answers the request and then quits; a connection that
    /// dies mid-response surfaces as an [`Error::Io`], which callers
    /// tearing a session down should tolerate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn stop_server(&self) -> Result<(), Error> {
        let result = self.call("server.stop", json!({})).await?;
        expect_ok(&result, "server.stop")
    }

    /// Opens a long-lived `events.subscribe` connection for `kinds` and
    /// returns a stream of matching events.
    ///
    /// Connects, sends the subscription request, and verifies the
    /// `subscription_started` acknowledgment before a background task takes
    /// over reading event lines. When the connection dies,
    /// [`EventStream::recv`] returns `None` and the caller should
    /// re-subscribe.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when connecting or subscribing fails.
    pub async fn subscribe(&self, subscriptions: &[Subscription]) -> Result<EventStream, Error> {
        let subscriptions: Vec<Value> = subscriptions
            .iter()
            .map(|subscription| {
                let mut value = json!({ "type": subscription.kind.subscription() });
                if let Some(pane_id) = &subscription.pane_id {
                    value["pane_id"] = json!(pane_id);
                } else if subscription.kind.requires_pane_scope() {
                    return Err(Error::Herdr {
                        code: "pane_scope_required".into(),
                        message: format!("{} requires a pane id", subscription.kind.subscription()),
                    });
                }
                Ok(value)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = json!({
            "id": REQUEST_ID,
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        let socket_path = self.socket_path.clone();
        let connect = async move {
            let mut stream = UnixStream::connect(&socket_path).await?;
            stream.write_all(line.as_bytes()).await?;
            Ok::<_, std::io::Error>(BufReader::new(stream))
        };
        let mut reader = tokio::time::timeout(self.operation_timeout, connect)
            .await
            .map_err(|_| Error::Timeout)??;

        let mut reader = tokio::time::timeout(self.operation_timeout, async move {
            loop {
                let mut line_buf = Vec::new();
                let read = reader.read_until(b'\n', &mut line_buf).await?;
                if read == 0 {
                    return Err(Error::Herdr {
                        code: "subscription_closed".into(),
                        message: "herdr closed the subscription connection before acknowledging"
                            .into(),
                    });
                }
                let envelope: Envelope = serde_json::from_slice(&line_buf)?;
                let result = envelope.into_result()?;
                if result.get("type").and_then(Value::as_str) == Some("subscription_started") {
                    break;
                }
            }
            Ok::<_, Error>(reader)
        })
        .await
        .map_err(|_| Error::Timeout)??;

        let (sender, receiver) = broadcast::channel(128);
        let reader_task = tokio::spawn(async move {
            loop {
                let mut line_buf = Vec::new();
                match reader.read_until(b'\n', &mut line_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(line) = serde_json::from_slice::<EventLine>(&line_buf) else {
                    continue;
                };
                let _ = sender.send(Event {
                    event: line.event,
                    data: line.data,
                });
            }
        });

        Ok(EventStream::new(receiver, reader_task))
    }
}

/// Errors from talking to the herdr socket.
#[derive(Debug, Error)]
pub enum Error {
    /// The herdr server reported a failure.
    #[error("herdr server error {code}: {message}")]
    Herdr {
        /// Machine-readable error code, e.g. `agent_not_found` or `timeout`.
        code: String,
        /// Human-readable error message.
        message: String,
    },
    /// The request exceeded the operation timeout.
    #[error("herdr request timed out")]
    Timeout,
    /// The herdr socket could not be reached or read.
    #[error("failed to communicate with herdr socket: {0}")]
    Io(#[from] std::io::Error),
    /// The response could not be parsed as the expected shape.
    #[error("failed to parse herdr response: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Returns `true` when the operation exceeded its allowed time: either the
    /// local operation timeout or a `timeout` error code from the herdr server
    /// (e.g. `agent prompt` still working after its wait deadline).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Herdr { code, .. } => code == "timeout",
            Self::Io(_) | Self::Json(_) => false,
        }
    }

    /// Whether the error is herdr's "prompt produced no observed state
    /// change" stall: the prompt was delivered, the agent just did not
    /// transition within herdr's short window. Callers keep waiting and
    /// let the transcript sync surface the message instead of failing.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        matches!(self, Self::Herdr { code, .. } if code == "agent_prompt_stalled")
    }
}

/// Request id for every request. herdr answers exactly one request per
/// connection, so ids only need to be unique per connection — one constant
/// suffices.
const REQUEST_ID: &str = "herdcord";

/// Converts a [`Duration`] to herdr's `timeout_ms` parameter.
fn timeout_ms(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)
}

/// Checks that a close response carries `type: "ok"`.
fn expect_ok(result: &Value, method: &str) -> Result<(), Error> {
    if result.get("type").and_then(Value::as_str) == Some("ok") {
        Ok(())
    } else {
        Err(Error::Herdr {
            code: "unexpected_response".into(),
            message: format!("`{method}` returned an unexpected response: {result}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn timeout_errors_report_is_timeout() {
        assert!(Error::Timeout.is_timeout());
        assert!(
            Error::Herdr {
                code: "timeout".into(),
                message: "still working".into(),
            }
            .is_timeout()
        );
        assert!(
            !Error::Herdr {
                code: "agent_not_found".into(),
                message: "no such agent".into(),
            }
            .is_timeout()
        );
    }
}

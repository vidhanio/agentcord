//! The socket client: the request machinery and one method per herdr API
//! call. The wire payload types live in `wire`; the data model in `model`.

use std::{path::PathBuf, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use super::{
    Agent, CreatedTab, CreatedWorkspace, Error, EventStream, PaneId, Subscription, TabId,
    WorkspaceId,
    wire::{
        AgentInfo, AgentList, SnapshotResult, TabCreated, WorkspaceCreated, WorkspaceList,
        WorktreeList,
    },
};
use crate::session::Harness;

/// Typed async client over the herdr Unix-socket API.
#[derive(Debug, Clone)]
pub struct Herdr {
    socket_path: PathBuf,
    operation_timeout: Duration,
    /// How long `agent.start` waits for the agent to be detected after the
    /// placeholder response.
    startup_timeout: Duration,
    /// How often `agent.start` polls for detection.
    startup_poll_interval: Duration,
}

impl Herdr {
    /// Creates a client that talks to the herdr Unix socket at `socket_path`.
    ///
    /// `operation_timeout` bounds each request's total runtime;
    /// `startup_timeout`/`startup_poll_interval` bound the `agent.start`
    /// detection wait.
    #[must_use]
    pub const fn new(
        socket_path: PathBuf,
        operation_timeout: Duration,
        startup_timeout: Duration,
        startup_poll_interval: Duration,
    ) -> Self {
        Self {
            socket_path,
            operation_timeout,
            startup_timeout,
            startup_poll_interval,
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
            let envelope: super::wire::Envelope = serde_json::from_slice(&line)?;
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
    pub async fn list_workspaces(&self) -> Result<Vec<super::Workspace>, Error> {
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

    /// Starts an agent of `harness` in `pane_id` under unique `name`,
    /// passing `agent_args` through to the agent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Herdr`], [`Error::Timeout`], [`Error::Io`], or
    /// [`Error::Json`] when the request fails.
    pub async fn start_agent(
        &self,
        name: &str,
        harness: Harness,
        pane_id: &PaneId,
        agent_args: &[String],
    ) -> Result<Agent, Error> {
        let info: AgentInfo = self
            .call_typed(
                "agent.start",
                json!({
                    "name": name,
                    "kind": harness.as_str(),
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
            if started.elapsed() >= self.startup_timeout {
                return Err(Error::Herdr {
                    code: "agent_startup_timeout".into(),
                    message: format!("timed out waiting for agent `{name}` to start"),
                });
            }
            tokio::time::sleep(self.startup_poll_interval).await;
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
    pub async fn worktree_list(&self, workspace_id: &WorkspaceId) -> Result<WorktreeList, Error> {
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
                let envelope: super::wire::Envelope = serde_json::from_slice(&line_buf)?;
                let result = envelope.into_result()?;
                if result.get("type").and_then(Value::as_str) == Some("subscription_started") {
                    break;
                }
            }
            Ok::<_, Error>(reader)
        })
        .await
        .map_err(|_| Error::Timeout)??;

        let (sender, receiver) = tokio::sync::broadcast::channel(128);
        let reader_task = tokio::spawn(async move {
            loop {
                let mut line_buf = Vec::new();
                match reader.read_until(b'\n', &mut line_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(line) = serde_json::from_slice::<super::EventLine>(&line_buf) else {
                    continue;
                };
                let _ = sender.send(super::event::Event {
                    event: line.event,
                    data: line.data,
                });
            }
        });

        Ok(EventStream::new(receiver, reader_task))
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

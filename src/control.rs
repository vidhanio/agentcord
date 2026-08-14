//! The `/herdr` one-shot control-plane agent: a throwaway agent in its own
//! named herdr session that loads the herdr skill, performs the action the
//! user typed, and replies with a short acknowledgment.
//!
//! The session is fully internal — its own server and socket under
//! `$XDG_CONFIG_HOME/herdr/sessions/<name>/`, never the main session the
//! bot mirrors — and is stopped and deleted after every run.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tokio::{net::UnixStream, process::Command, time::sleep};
use tracing::{info, warn};

use crate::{
    BotResult,
    config::{self, DEFAULT_AGENT_KIND, OPERATION_TIMEOUT},
    herdr::{Agent, Herdr, PaneId},
    session::{SessionRole, read_session_messages},
};

/// The workspace and agent name inside the throwaway session. The session
/// name itself comes from [`config::CONTROL_SESSION_NAME`].
const CONTROL_WORKSPACE_LABEL: &str = "herdcord";

/// How long the whole settle wait may take before the run is abandoned.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to wait for the throwaway session's server to come up.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the server to stop before deleting its session dir.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the socket liveness probes poll.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The injected prompt for the control-plane agent: perform the user's
/// herdr action via the herdr skill, then acknowledge briefly.
#[must_use]
pub fn control_prompt(user_text: &str) -> String {
    format!(
        "You are herdcord's herdr control plane, running inside a throwaway herdr session. \
         Use the herdr skill to perform the following action:\n\n{user_text}\n\n\
         When the action is done, reply with a single short plain-text message acknowledging \
         that you completed it. If the request is impossible or ambiguous, reply with a \
         one-line explanation instead. Do not explain your reasoning or dump tool output."
    )
}

/// Runs a one-shot control-plane agent in a throwaway herdr session.
///
/// Spawns the session's server headlessly, starts an agent with
/// [`control_prompt`] as its prompt, waits for it to settle, and returns
/// the agent's final reply.
///
/// The session is stopped and deleted afterwards whether the run succeeds
/// or fails, so a crashed run never leaves a stray server behind.
pub async fn run_control_agent(session_name: &str, user_text: &str) -> BotResult<String> {
    let socket = config::session_socket_path(session_name);

    // A leftover session (a crashed earlier run) is stopped and wiped
    // before the fresh one starts; the same cleanup runs at the end.
    cleanup_session(session_name, &socket).await;
    let result = spawn_and_run(&socket, user_text).await;
    cleanup_session(session_name, &socket).await;
    result
}

/// Spawns the session's headless server, waits for its API socket, and
/// runs the agent.
async fn spawn_and_run(socket: &Path, user_text: &str) -> BotResult<String> {
    spawn_server(socket).await?;
    wait_for_socket(socket, SPAWN_TIMEOUT).await?;

    let herdr = Herdr::new(socket.to_owned(), OPERATION_TIMEOUT);
    let cwd = dirs::home_dir().map_or_else(
        || "/tmp".to_owned(),
        |dir| dir.to_string_lossy().into_owned(),
    );
    let created = herdr
        .create_workspace_with_pane(CONTROL_WORKSPACE_LABEL, &cwd)
        .await?;
    let agent = herdr
        .start_agent(
            CONTROL_WORKSPACE_LABEL,
            DEFAULT_AGENT_KIND.as_str(),
            &created.pane_id,
            &[],
        )
        .await?;

    herdr
        .send_prompt(&agent.pane_id, &control_prompt(user_text))
        .await?;
    let agent = wait_until_settled(&herdr, &agent.pane_id).await?;

    Ok(acknowledgment(&agent))
}

/// Waits for the agent to settle (idle/done/blocked), bounded by
/// [`CONTROL_TIMEOUT`].
async fn wait_until_settled(herdr: &Herdr, target: &PaneId) -> BotResult<Agent> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    loop {
        match herdr.wait_agent(target, config::PROMPT_TIMEOUT).await {
            Ok(agent) => return Ok(agent),
            Err(error) if error.is_timeout() && Instant::now() < deadline => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// The control agent's final reply: the last agent message in its
/// transcript, or a generic note when no session or message was recorded.
#[must_use]
fn acknowledgment(agent: &Agent) -> String {
    let Some(session) = agent.agent_session.as_ref() else {
        return "the control agent finished without a recorded reply.".to_owned();
    };
    let reply = read_session_messages(DEFAULT_AGENT_KIND, session.value.as_str())
        .ok()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.role == SessionRole::Agent)
                .map(|message| message.text.clone())
        })
        .unwrap_or_else(|| "the control agent finished without a recorded reply.".to_owned());
    reply.chars().take(1900).collect()
}

/// Spawns the named session's headless server, fully detached (`setsid
/// -f`), with the session selected by env and any socket overrides cleared
/// so the server binds its own session socket.
async fn spawn_server(socket: &Path) -> BotResult<()> {
    let session_name = session_name_of(socket)?;
    let status = Command::new("setsid")
        .arg("-f")
        .arg("env")
        .arg("-u")
        .arg("HERDR_SOCKET_PATH")
        .arg("-u")
        .arg("HERDR_CLIENT_SOCKET_PATH")
        .arg(format!("HERDR_SESSION={session_name}"))
        .arg("herdr")
        .arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|error| {
            crate::BotError::Other(format!(
                "failed to spawn the herdr server for session `{session_name}`: {error}"
            ))
        })?;
    if !status.success() {
        return Err(crate::BotError::Other(format!(
            "the herdr server for session `{session_name}` failed to start"
        )));
    }
    Ok(())
}

/// The session name for a `sessions/<name>/herdr.sock` socket path.
fn session_name_of(socket: &Path) -> BotResult<String> {
    socket
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            crate::BotError::Other(format!(
                "cannot derive the session name from socket path {}",
                socket.display()
            ))
        })
}

/// Waits until `socket` accepts connections or `timeout` elapses.
async fn wait_for_socket(socket: &Path, timeout: Duration) -> BotResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket).await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => sleep(POLL_INTERVAL).await,
            Err(error) => {
                return Err(crate::BotError::Other(format!(
                    "herdr session server at {} did not come up: {error}",
                    socket.display()
                )));
            }
        }
    }
}

/// Stops the session's server (if one is running), waits for its socket
/// to die, and deletes the session directory. Best-effort: every step is
/// logged, never fatal.
async fn cleanup_session(session_name: &str, socket: &Path) {
    // Not running (a fresh start, or an already-dead crash leftover) fails
    // with a connection error — expected, so not logged.
    let _ = Herdr::new(socket.to_owned(), OPERATION_TIMEOUT)
        .stop_server()
        .await;

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline && UnixStream::connect(socket).await.is_ok() {
        sleep(POLL_INTERVAL).await;
    }

    let dir = session_dir(session_name);
    if let Err(error) = tokio::fs::remove_dir_all(&dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            %session_name,
            ?error,
            "failed to delete control session directory"
        );
    } else {
        info!(%session_name, "deleted control session");
    }
}

/// The session's directory under herdr's config dir.
fn session_dir(session_name: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("herdr")
        .join("sessions")
        .join(session_name)
}

#[cfg(test)]
mod tests {
    use super::{acknowledgment, control_prompt};
    use crate::herdr::{Agent, AgentSession, PaneId, SessionPath, TabId, WorkspaceId};

    #[test]
    fn control_prompt_carries_the_action_and_style() {
        let prompt = control_prompt("list all workspaces");
        assert!(prompt.contains("list all workspaces"));
        assert!(prompt.contains("herdr skill"));
        assert!(prompt.contains("acknowledging"));
        assert!(prompt.contains("one-line"));
    }

    #[test]
    fn acknowledgment_returns_last_agent_message() {
        let agent = Agent {
            agent: Some("omp".to_owned()),
            agent_status: "idle".to_owned(),
            name: Some("herdcord".to_owned()),
            pane_id: PaneId::from("w1:p1"),
            tab_id: TabId::from("w1:t1"),
            workspace_id: WorkspaceId::from("w1"),
            cwd: "/tmp".into(),
            focused: false,
            launch_pending: false,
            terminal_title_stripped: None,
            agent_session: Some(AgentSession {
                agent: "omp".to_owned(),
                kind: "jsonl".to_owned(),
                source: "file".to_owned(),
                value: SessionPath::from("/nonexistent-transcript.jsonl"),
            }),
        };
        // No readable transcript: generic note, not a panic.
        assert!(acknowledgment(&agent).contains("without a recorded reply"));
    }
}

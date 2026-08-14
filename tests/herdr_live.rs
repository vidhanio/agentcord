//! Live integration tests for the herdr socket client.
//!
//! These tests talk to the real herdr server and spawn real agents, so they
//! are gated behind `HERDR_LIVE_TESTS=1` and skipped otherwise.

use std::{path::Path, process::Command, time::Duration};

use herdcord::{
    AgentKind,
    config::socket_path,
    herdr::{AgentStatus, EventKind, Herdr, Subscription},
    read_session,
};

/// Closes a workspace by calling the herdr CLI synchronously, so cleanup
/// works even while the tokio runtime is unwinding or shutting down.
fn close_workspace_sync(workspace_id: &str) {
    let _ = Command::new("herdr")
        .args(["workspace", "close", workspace_id])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

struct WorkspaceGuard {
    workspace_id: String,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        close_workspace_sync(&self.workspace_id);
    }
}

/// Closes every workspace whose label starts with `prefix`, sweeping up
/// workspaces leaked by earlier killed runs.
async fn sweep_workspaces(herdr: &Herdr, prefix: &str) {
    let workspaces = herdr.list_workspaces().await.expect("list workspaces");
    for workspace in workspaces {
        if workspace.label.starts_with(prefix) {
            close_workspace_sync(workspace.workspace_id.as_str());
        }
    }
}

#[tokio::test]
async fn spawn_prompt_read_close_roundtrip() {
    if std::env::var("HERDR_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }

    let herdr = Herdr::new(socket_path(), Duration::from_secs(30));

    // Sweep up workspaces leaked by earlier killed runs.
    sweep_workspaces(&herdr, "herdcord-live-").await;

    let pid = std::process::id();
    let created = herdr
        .create_workspace_with_pane(&format!("herdcord-live-{pid}"), "/tmp")
        .await
        .expect("create workspace with pane");
    let guard = WorkspaceGuard {
        workspace_id: created.workspace.workspace_id.to_string(),
    };

    let name = format!("bottest{pid}");
    let agent = herdr
        .start_agent(&name, "omp", &created.pane_id, &[])
        .await
        .expect("start agent");
    assert_eq!(agent.name.as_deref(), Some(name.as_str()));
    assert_eq!(agent.cwd, std::path::PathBuf::from("/tmp"));
    assert!(!matches!(agent.status(), AgentStatus::Unknown));

    // Delivery and settlement are separate calls — the same two-step flow
    // the relay uses, so a long turn never holds its queue: the prompt
    // goes to the agent immediately, and the settle is waited for after.
    let _ = herdr
        .send_prompt(&agent.pane_id, "Reply with exactly: OK")
        .await
        .expect("send prompt");
    let agent = herdr
        .wait_agent(&agent.pane_id, Duration::from_secs(90))
        .await
        .expect("wait for settle");
    assert!(!matches!(agent.status(), AgentStatus::Working));

    let status = herdr.get_agent(&agent.pane_id).await.expect("get agent");
    assert_eq!(status.name.as_deref(), Some(name.as_str()));

    herdr.close_tab(&agent.tab_id).await.expect("close tab");
    drop(guard);
}

/// The one-shot control-plane runner spawns its own throwaway herdr
/// session, runs the agent on the user's action, and returns its
/// acknowledgment — and the session is stopped and deleted afterwards.
#[tokio::test]
async fn control_agent_runs_in_throwaway_session() {
    if std::env::var("HERDR_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }

    let session_name = format!("herdcord-control-{}", std::process::id());
    let socket = herdcord::config::session_socket_path(&session_name);

    let acknowledgment = herdcord::control::run_control_agent(
        &session_name,
        "list the herdr workspaces and reply with how many there are",
    )
    .await
    .expect("control agent runs");
    assert!(
        !acknowledgment.trim().is_empty(),
        "control agent produced no acknowledgment"
    );

    // The one-shot session was torn down: its socket and session dir are
    // gone, so a second run starts from a clean slate.
    assert!(
        !socket.exists(),
        "control session socket still exists after the run"
    );
}

/// Subscribing to `pane.updated` delivers an event for a freshly started
/// agent's pane.
#[tokio::test]
async fn event_stream_delivers_pane_updates() {
    if std::env::var("HERDR_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }

    let herdr = Herdr::new(socket_path(), Duration::from_secs(30));

    // Sweep up workspaces leaked by earlier killed runs. The prefix is
    // disjoint from the roundtrip test's so the two tests never close each
    // other's workspaces while running in parallel.
    sweep_workspaces(&herdr, "herdcord-events-").await;

    let pid = std::process::id();
    let created = herdr
        .create_workspace_with_pane(&format!("herdcord-events-{pid}"), "/tmp")
        .await
        .expect("create workspace with pane");
    let guard = WorkspaceGuard {
        workspace_id: created.workspace.workspace_id.to_string(),
    };

    let name = format!("bottest{pid}events");
    let agent = herdr
        .start_agent(&name, "omp", &created.pane_id, &[])
        .await
        .expect("start agent");

    let mut stream = herdr
        .subscribe(&[Subscription::for_pane(
            EventKind::PaneAgentStatusChanged,
            agent.pane_id.clone(),
        )])
        .await
        .expect("subscribe to status changes");

    // Generate output so the agent's pane emits update events; a settled
    // pane is quiet and would not. Drain the stream WHILE the prompt runs —
    // the internal buffer is small and this session's own pane floods it.
    let prompt_herdr = herdr.clone();
    let prompt_pane = agent.pane_id.clone();
    let prompt = tokio::spawn(async move {
        prompt_herdr
            .prompt_agent(
                &prompt_pane,
                "Reply with exactly: OK",
                Duration::from_secs(90),
            )
            .await
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut matched = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(event))
                if event.pane_id().as_ref().map(|id| id.as_str())
                    == Some(agent.pane_id.as_str())
                    && matches!(event.kind(), Some(EventKind::PaneAgentStatusChanged)) =>
            {
                matched = true;
                break;
            }
            Ok(Some(_)) => {}
            // The subscription died; nothing more will arrive.
            Ok(None) | Err(_) => break,
        }
    }

    prompt.await.expect("prompt task").expect("prompt agent");
    assert!(matched, "no status change for {}", agent.pane_id);

    herdr.close_tab(&agent.tab_id).await.expect("close tab");
    drop(guard);
}

/// A started agent's session file records the conversation, and the
/// session parser reads it back into a normalized transcript.
#[tokio::test]
async fn session_file_records_conversation() {
    if std::env::var("HERDR_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }

    let herdr = Herdr::new(socket_path(), Duration::from_secs(30));

    // Sweep up workspaces leaked by earlier killed runs. The prefix is
    // disjoint from the other tests' so they never close each other's
    // workspaces while running in parallel.
    sweep_workspaces(&herdr, "herdcord-session-").await;

    let pid = std::process::id();
    let created = herdr
        .create_workspace_with_pane(&format!("herdcord-session-{pid}"), "/tmp")
        .await
        .expect("create workspace with pane");
    let guard = WorkspaceGuard {
        workspace_id: created.workspace.workspace_id.to_string(),
    };

    let name = format!("bottest{pid}session");
    let agent = herdr
        .start_agent(&name, "omp", &created.pane_id, &[])
        .await
        .expect("start agent");

    herdr
        .prompt_agent(
            &agent.pane_id,
            "Reply with exactly: SESSIONTEST",
            Duration::from_secs(90),
        )
        .await
        .expect("prompt agent");

    // The settled turn has flushed the transcript, so the agent now reports
    // its session path and the file on disk contains the prompt.
    let session = herdr
        .get_agent(&agent.pane_id)
        .await
        .expect("get agent")
        .agent_session
        .expect("agent reports a session");

    let messages =
        read_session(AgentKind::Omp, Path::new(session.value.as_str())).expect("read session file");
    assert!(
        messages
            .iter()
            .any(|message| message.text.contains("SESSIONTEST")),
        "session transcript is missing the prompt: {messages:?}"
    );

    herdr.close_tab(&agent.tab_id).await.expect("close tab");
    drop(guard);
}

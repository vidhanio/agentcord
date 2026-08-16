//! The `/herdr` control command: a user-configured one-shot subprocess.
//!
//! The command (e.g. a lean `pi -p`) is configured via
//! `herdr.control_command`; the user's prompt, prefixed with a
//! control-plane preamble ([`control_prompt`]), is piped to its stdin,
//! and its concatenated output is relayed back as the reply
//! ([`truncate_reply`]). The caller injects `HERDR_ENV=1` and the
//! resolved herdr socket path so the command can act on the main herdr
//! session — the one the forums mirror. No herdr session is spawned; the
//! process is one-shot and stateless.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::io::AsyncWriteExt;

use crate::BotError;

/// The prompt piped to the control command: a one-shot preamble that
/// frames the session and the confirmation contract, followed by the
/// user's request.
#[must_use]
pub fn control_prompt(user_text: &str) -> String {
    format!(
        "You are herdcord's herdr control plane, invoked from Discord. \
         This is a one-shot session: it exists only to perform the user's \
         requested herdr action against the main herdr session.\n\n\
         Start by reading the herdr skill (`herdr --skill`), then perform \
         the action. Fire off the herdr commands and ensure they complete \
         successfully — do not monitor agent output or wait for turns.\n\n\
         When done, reply with a single short plain-text message confirming \
         what you did. If the request is impossible or ambiguous, reply with \
         a one-line explanation instead. Do not explain your reasoning or \
         dump tool output.\n\n\
         User request:\n{user_text}"
    )
}

/// Truncates the control command's output to at most `limit` characters
/// (Discord's per-message cap counts characters), appending a truncation
/// note when it was cut. The cut never splits a UTF-8 character.
#[must_use]
pub fn truncate_reply(output: &str, limit: usize) -> String {
    if output.chars().count() <= limit {
        return output.to_owned();
    }
    let note = "\n… (truncated)";
    let keep = limit.saturating_sub(note.chars().count());
    let mut truncated = output.chars().take(keep).collect::<String>();
    truncated.push_str(note);
    truncated
}

/// Runs the control command: spawns `command` (whitespace-split into
/// argv) with `prompt` piped to its stdin, waits up to `timeout`, and
/// returns the concatenated stdout and stderr.
///
/// On timeout the command's whole process group is killed — the command
/// runs in its own group, so its descendants die with it.
///
/// `extra_env` overrides the inherited environment; the caller injects
/// `HERDR_ENV=1` and the resolved herdr socket path here.
pub async fn run_control_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    prompt: &str,
    extra_env: &[(&str, String)],
) -> Result<String, BotError> {
    let mut parts = command.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| BotError::Other("the control command is empty".into()))?;
    let args = parts.collect::<Vec<_>>();
    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .current_dir(cwd)
        .envs(extra_env.iter().map(|(name, value)| (*name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| BotError::Other(format!("couldn't spawn the control command: {error}")))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    // The child may exit without reading stdin (a bad command); a
    // broken pipe is not a failure of the run itself. The write runs
    // inside the timeout so a child that never reads stdin cannot
    // outlive the contract — the timeout kills the group and closes the
    // pipe.
    let write_stdin = async {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
    };
    let output = if let Ok(result) = tokio::time::timeout(timeout, async {
        tokio::join!(write_stdin, child.wait_with_output()).1
    })
    .await
    {
        result.map_err(|error| {
            BotError::Other(format!("the control command's I/O failed: {error}"))
        })?
    } else {
        kill_process_group(pid).await;
        return Err(BotError::Other(format!(
            "the control command timed out after {timeout:?}"
        )));
    };

    let mut reply = String::from_utf8_lossy(&output.stdout).into_owned();
    reply.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(reply)
}

/// Sends SIGTERM to the process group led by `pid` (the control command
/// runs in its own group, so this reaches its descendants too). Best
/// effort: the leader itself is covered by the child handle's
/// `kill_on_drop` backstop.
async fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let _ = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{pid}"))
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use super::{control_prompt, run_control_command, truncate_reply};

    #[test]
    fn control_prompt_frames_the_user_request() {
        let prompt = control_prompt("close workspace foo");
        assert!(prompt.contains("one-shot"), "{prompt}");
        assert!(prompt.contains("herdr --skill"), "{prompt}");
        assert!(prompt.contains("do not monitor agent output"), "{prompt}");
        assert!(
            prompt.ends_with("User request:\nclose workspace foo"),
            "{prompt}"
        );
    }

    #[test]
    fn truncate_reply_passes_short_output_through() {
        assert_eq!(truncate_reply("done", 2000), "done");
    }

    #[test]
    fn truncate_reply_cuts_long_output_at_the_cap() {
        let long = "x".repeat(5000);
        let truncated = truncate_reply(&long, 2000);
        assert_eq!(truncated.chars().count(), 2000);
        assert!(truncated.ends_with("… (truncated)"));
    }

    #[test]
    fn truncate_reply_counts_characters_not_bytes() {
        let long = "é".repeat(5000);
        let truncated = truncate_reply(&long, 2000);
        assert_eq!(truncated.chars().count(), 2000);
        assert!(truncated.ends_with("… (truncated)"));
    }

    #[tokio::test]
    async fn run_control_command_pipes_the_prompt_and_returns_stdout() {
        let output = run_control_command(
            "cat",
            Path::new("."),
            Duration::from_secs(5),
            "hello stdin",
            &[],
        )
        .await
        .expect("cat run succeeds");
        assert_eq!(output, "hello stdin");
    }

    #[tokio::test]
    async fn run_control_command_concatenates_stderr() {
        let output = run_control_command(
            "sh -c cat>&2",
            Path::new("."),
            Duration::from_secs(5),
            "oops",
            &[],
        )
        .await
        .expect("sh run succeeds");
        assert_eq!(output, "oops");
    }

    #[tokio::test]
    async fn run_control_command_applies_the_extra_env() {
        let output = run_control_command(
            "printenv HERDR_TEST_VAR",
            Path::new("."),
            Duration::from_secs(5),
            "",
            &[("HERDR_TEST_VAR", "injected".to_owned())],
        )
        .await
        .expect("printenv run succeeds");
        assert_eq!(output.trim(), "injected");
    }

    #[tokio::test]
    async fn run_control_command_returns_output_on_nonzero_exit() {
        let output = run_control_command(
            "sh -c false",
            Path::new("."),
            Duration::from_secs(5),
            "",
            &[],
        )
        .await
        .expect("nonzero exit still returns output");
        assert_eq!(output, "");
    }

    #[tokio::test]
    async fn run_control_command_kills_the_process_group_on_timeout() {
        let started = std::time::Instant::now();
        let error = run_control_command(
            "sleep 30",
            Path::new("."),
            Duration::from_millis(100),
            "",
            &[],
        )
        .await
        .expect_err("sleep outlives the timeout");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed promptly"
        );
        let message = error.to_string();
        assert!(message.contains("timed out"), "{message}");
    }

    #[tokio::test]
    async fn run_control_command_rejects_an_empty_command() {
        let error = run_control_command("   ", Path::new("."), Duration::from_secs(5), "", &[])
            .await
            .expect_err("empty command");
        let message = error.to_string();
        assert!(message.contains("empty"), "{message}");
    }

    #[tokio::test]
    async fn run_control_command_reports_spawn_failures() {
        let error = run_control_command(
            "definitely-not-a-real-command-xyz",
            Path::new("."),
            Duration::from_secs(5),
            "",
            &[],
        )
        .await
        .expect_err("missing program");
        let message = error.to_string();
        assert!(message.contains("couldn't spawn"), "{message}");
    }
}

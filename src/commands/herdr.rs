//! The `/herdr` command: runs the configured `herdr_control_command` as a
//! one-shot control plane against the main herdr session.

use poise::serenity_prelude as serenity;
use serenity::EditInteractionResponse;

use crate::{Bot, BotError, control};

/// run a one-shot control command against the main herdr session.
///
/// The command is the configured `herdr_control_command` (e.g. a lean
/// `pi -p`); the prompt, prefixed with a control-plane preamble, is piped
/// to its stdin, and its output is relayed back, truncated to Discord's
/// message cap. The command runs with `HERDR_ENV=1` and the bot's
/// resolved herdr socket injected, so it acts on the main session — the
/// one the forums mirror.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn herdr(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "what the control command should do"] prompt: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    // `build_commands` registers `/herdr` only when `herdr_control_command`
    // is set, but a failed `register_in_guild` at startup can leave a
    // stale `/herdr` in the guild after the config changed — so the guard
    // below is reachable in practice, not just defensive.
    let Some(command) = bot.config.herdr_control_command.clone() else {
        ctx.send(
            poise::CreateReply::new()
                .content("the control command is not configured.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    };
    if prompt.trim().is_empty() {
        ctx.send(
            poise::CreateReply::new()
                .content("the prompt is empty.")
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    // The command may run for the whole control timeout; defer so the
    // interaction's 3-second response window is never missed, then edit
    // the deferred response with the outcome.
    ctx.defer_ephemeral().await?;

    let socket = bot.config.socket_path();
    let extra_env = [
        ("HERDR_ENV", "1".to_owned()),
        ("HERDR_SOCKET_PATH", socket.to_string_lossy().into_owned()),
    ];
    let prompt = control::control_prompt(&prompt);
    let cwd = bot.config.control_cwd();
    let timeout = bot.config.control_timeout();
    let outcome = control::run_control_command(&command, &cwd, timeout, &prompt, &extra_env).await;
    let reply = match outcome {
        Ok(output) => {
            let reply = control::truncate_reply(&output, bot.config.control_reply_limit);
            if reply.trim().is_empty() {
                "the control command produced no output.".to_owned()
            } else {
                reply
            }
        }
        Err(error) => format!("control command failed: {error}"),
    };
    ctx.interaction
        .edit_response(ctx.http(), EditInteractionResponse::new().content(reply))
        .await?;
    Ok(())
}

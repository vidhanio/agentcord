use serenity::model::mention::Mentionable;
use tracing::{debug, info};

use crate::{Bot, BotError};

/// Reloads the persisted ACP session for the current Discord thread.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn reload(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let thread = ctx.channel_id();
    let user = ctx.author().id;
    info!(%user, ?thread, "reloading session...");
    debug!(%user, ?thread, "deferring session reload response...");
    ctx.defer_ephemeral().await?;
    debug!(%user, ?thread, "deferred session reload response");
    let reloaded_thread = bot.reload_session(thread).await?;
    debug!(
        %user,
        ?thread,
        ?reloaded_thread,
        "sending session reload response..."
    );
    ctx.send(
        poise::CreateReply::new()
            .content(format!("reloaded {}", reloaded_thread.mention()))
            .ephemeral(true),
    )
    .await?;
    debug!(%user, ?thread, ?reloaded_thread, "sent session reload response");
    info!(%user, ?thread, ?reloaded_thread, "session reloaded");
    Ok(())
}

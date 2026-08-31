use serenity::model::mention::Mentionable;
use tracing::info;

use crate::{Bot, BotError};

/// Reloads the persisted ACP session for the current Discord thread.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn reload(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let thread = ctx.channel_id();
    let user = ctx.author().id;
    info!(%user, ?thread, "reloading session...");
    ctx.defer_ephemeral().await?;
    let reloaded_thread = bot.reload_session(thread).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("reloaded {}", reloaded_thread.mention()))
            .ephemeral(true),
    )
    .await?;
    info!(%user, ?thread, ?reloaded_thread, "session reloaded");
    Ok(())
}

use serenity::model::mention::Mentionable;
use tracing::info;

use crate::{Bot, BotError};

/// Refreshes the current Discord thread from a fresh ACP session load.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn refresh(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let thread = ctx.channel_id();
    let user = ctx.author().id;
    info!(%user, ?thread, "refreshing session...");
    ctx.defer_ephemeral().await?;
    let refreshed_thread = bot.refresh_session(thread).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("refreshed {}", refreshed_thread.mention()))
            .ephemeral(true),
    )
    .await?;
    info!(%user, ?thread, ?refreshed_thread, "session refreshed");
    Ok(())
}

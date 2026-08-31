use serenity::model::mention::Mentionable;
use tracing::info;

use crate::{Bot, BotError};

/// Recreates the persisted ACP session for the current Discord thread.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn recreate(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let thread = ctx.channel_id();
    let user = ctx.author().id;
    info!(%user, ?thread, "recreating session...");
    ctx.defer_ephemeral().await?;
    let recreated_thread = bot.recreate_session(thread).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("recreated {}", recreated_thread.mention()))
            .ephemeral(true),
    )
    .await?;
    info!(%user, ?thread, ?recreated_thread, "session recreated");
    Ok(())
}

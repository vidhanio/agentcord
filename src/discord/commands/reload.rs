use serenity::model::mention::Mentionable;

use crate::{Bot, BotError};

/// Reloads the persisted ACP session for the current Discord thread.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn reload(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let thread = ctx.channel_id();
    ctx.defer_ephemeral().await?;
    bot.reload_session(thread).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("reloaded {}", thread.mention()))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

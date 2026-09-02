use serenity::all::{GetMessages, MessageId};
use tracing::info;

use crate::{Bot, BotError};

/// Replies with the contents of the first message in the current thread.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn session(ctx: poise::ApplicationContext<'_, Bot, BotError>) -> Result<(), BotError> {
    let bot = ctx.data();
    let thread = ctx.channel_id();
    info!(?thread, "reading session starter message...");
    ctx.defer_ephemeral().await?;
    let discord_context = bot.context()?.clone();
    let messages = thread
        .messages(
            &discord_context.http,
            GetMessages::new().limit(1).after(MessageId::new(1)),
        )
        .await?;
    let content = messages.first().map_or_else(
        || String::from("this thread has no messages"),
        |message| message.content.to_string(),
    );
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

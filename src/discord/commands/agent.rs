use std::path::{Path, PathBuf};

use serenity::model::mention::Mentionable;
use tracing::warn;

use crate::{Bot, BotError};

/// Creates a new ACP session in a configured project.
#[poise::command(
    slash_command,
    install_context = "Guild|User",
    interaction_context = "Guild|BotDm|PrivateChannel",
    check = "super::allowed"
)]
pub async fn agent(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "configured agent"] agent: usize,
    #[description = "absolute or home-relative project directory"] project: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let agent_key = super::agent_key_at(&bot, agent)?;
    let project = resolve_project_path(project.trim())?;
    ctx.defer_ephemeral().await?;
    let operation_bot = bot.clone();
    let operation_agent = agent_key.clone();
    let operation = tokio::spawn(async move {
        operation_bot
            .create_session(&operation_agent, project)
            .await
    });
    let thread = match operation.await {
        Ok(Ok(thread)) => thread,
        Ok(Err(error)) => {
            warn!(?error, "failed to create session");
            ctx.send(
                poise::CreateReply::new()
                    .content(format!("couldn't create the session: {error}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
        Err(error) => {
            warn!(?error, "session creation task failed");
            ctx.send(
                poise::CreateReply::new()
                    .content(format!("couldn't create the session: {error}"))
                    .ephemeral(true),
            )
            .await?;
            return Ok(());
        }
    };
    let content = format!("created {}", thread.mention());
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

/// Resolves a user-supplied project path after expanding a leading `~`.
fn resolve_project_path(input: &str) -> Result<PathBuf, BotError> {
    let path = expand_home(Path::new(input))?;
    let canonical = path
        .canonicalize()
        .map_err(|source| BotError::ProjectPathResolution {
            path: path.clone(),
            source,
            description: "project directory".to_owned(),
        })?;
    if !canonical.is_dir() {
        return Err(BotError::ProjectNotDirectory { path: canonical });
    }
    Ok(canonical)
}

/// Expands a leading `~` using the current user's home directory.
fn expand_home(path: &Path) -> Result<PathBuf, BotError> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let home = dirs::home_dir().ok_or(BotError::HomeDirectoryUnavailable)?;
        return Ok(if text == "~" {
            home
        } else {
            home.join(&text[2..])
        });
    }
    Ok(path.to_owned())
}

use std::path::{Path, PathBuf};

use serenity::model::mention::Mentionable;
use tracing::{info, warn};

use crate::{Bot, BotError, discord::expand_home};

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
    #[description = "project directory or base-relative project name"] project: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let agent_key = super::agent_key_at(&bot, agent)?;
    let project = resolve_project_path(project.trim(), &bot.config().projects.base_path)?;
    let user = ctx.author().id;
    let channel = ctx.channel_id();
    info!(
        %user,
        ?channel,
        agent = %agent_key,
        project = ?project,
        "creating session..."
    );
    ctx.defer_ephemeral().await?;
    let thread = match bot.create_session(&agent_key, project).await {
        Ok(thread) => {
            info!(%user, ?thread, agent = %agent_key, "session created");
            thread
        }
        Err(error) => {
            warn!(?error, %user, agent = %agent_key, "failed to create session");
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

/// Resolves a user-supplied project path after expanding `~` and the project
/// base path for relative selections.
fn resolve_project_path(input: &str, base_path: &Path) -> Result<PathBuf, BotError> {
    let path = expand_home(Path::new(input))?;
    let path = if path.is_relative() && !base_path.as_os_str().is_empty() {
        expand_home(base_path)?.join(path)
    } else {
        path
    };
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::resolve_project_path;

    #[test]
    fn resolves_relative_project_below_base_path() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let base = std::env::temp_dir().join(format!("agentcord-projects-{suffix}"));
        let project = base.join("agentcord");
        fs::create_dir_all(&project).expect("create project directory");

        let resolved = resolve_project_path("agentcord", &base).expect("resolve project");

        assert_eq!(resolved, project.canonicalize().expect("canonical project"));
        fs::remove_dir_all(&base).expect("remove project directory");
    }
}

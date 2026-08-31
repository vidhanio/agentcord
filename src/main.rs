use std::{env, path::PathBuf};

use agentcord::Config;
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// A Discord client for ACP agents.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file path.
    #[arg(short, long, env = "AGENTCORD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
/// Loads configuration, initializes logging, and starts the Discord client.
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(agentcord::config::config_path);
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var(EnvFilter::DEFAULT_ENV)
                .as_deref()
                .unwrap_or("warn,agentcord=trace"),
        )
        .compact()
        .init();

    info!(path = ?path, "starting agentcord...");
    info!(path = ?path, "loading configuration...");
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            error!(?error, path = ?path, "failed to load configuration");
            return Err(error.into());
        }
    };
    info!(agents = config.agents.len(), "configuration loaded");
    agentcord::run(config).await?;
    info!("agentcord stopped");
    Ok(())
}

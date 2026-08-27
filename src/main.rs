use std::{env, path::PathBuf};

use agentcord::Config;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// A Discord client for Agent Client Protocol agents.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the config file (default:
    /// `$XDG_CONFIG_HOME/agentcord/config.toml`).
    #[arg(short, long, env = "AGENTCORD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(agentcord::config::config_path);
    let config = Config::load(&path)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var(EnvFilter::DEFAULT_ENV)
                .as_deref()
                .unwrap_or("warn,agentcord=trace"),
        )
        .compact()
        .init();
    agentcord::run(config).await?;
    Ok(())
}

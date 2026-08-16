use std::{env, path::PathBuf};

use clap::Parser;
use herdcord::Config;
use tracing_subscriber::EnvFilter;

/// herdcord: a discord bot mirroring herdr agent sessions into forums.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// path to the config file (default:
    /// `$XDG_CONFIG_HOME/herdcord/config.toml`)
    #[arg(short, long, env = "HERDCORD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(herdcord::config::config_path);
    let config = Config::load(&path)?;

    let fmt = tracing_subscriber::fmt()
        .with_env_filter(
            env::var(EnvFilter::DEFAULT_ENV)
                .as_deref()
                .unwrap_or("warn,herdcord=trace"),
        )
        .compact();
    fmt.init();

    herdcord::run(config).await?;

    Ok(())
}

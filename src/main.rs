use std::env;

use herdcord::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    _ = dotenvy::dotenv();

    let config = Config::from_env()?;

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

//! Shared test fixtures (test-only module; never compiled into the bot).

use std::path::PathBuf;

use serenity::all::GuildId;

use crate::config::Config;

/// A config with the `/herdr` control knobs set as given; the rest is
/// dummy. The control-command tests in `config.rs` and `commands.rs`
/// share this so a new `Config` field needs one edit, not two.
pub fn control_config(command: Option<&str>, cwd: Option<PathBuf>, timeout: Option<u64>) -> Config {
    Config {
        discord_bot_token: "token".into(),
        guild_id: GuildId::new(1),
        allowed_user_id: None,
        herdr_control_command: command.map(str::to_owned),
        herdr_control_cwd: cwd,
        herdr_control_timeout: timeout,
    }
}

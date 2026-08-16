//! Shared test fixtures (test-only module; never compiled into the bot).

use std::{path::PathBuf, time::Duration};

use serenity::all::GuildId;

use crate::config::{
    CATCHUP_BACKLOG, CONTROL_REPLY_LIMIT, CONTROL_TIMEOUT, Config, DEFAULT_HARNESS, Delays,
    MAX_SYNC_MESSAGES,
};

/// A config with the `/herdr` control knobs set as given; the rest is
/// dummy. The control-command tests in `config.rs` and `commands.rs`
/// share this so a new `Config` field needs one edit, not two.
pub fn control_config(
    command: Option<&str>,
    cwd: Option<PathBuf>,
    timeout: Option<Duration>,
) -> Config {
    Config {
        discord_bot_token: "token".into(),
        guild_id: GuildId::new(1),
        allowed_user_id: None,
        default_harness: DEFAULT_HARNESS,
        herdr_socket_path: None,
        herdr_session: None,
        herdr_control_command: command.map(str::to_owned),
        herdr_control_cwd: cwd,
        herdr_control_timeout: timeout.unwrap_or(CONTROL_TIMEOUT),
        control_reply_limit: CONTROL_REPLY_LIMIT,
        max_sync_messages: MAX_SYNC_MESSAGES,
        catchup_backlog: CATCHUP_BACKLOG,
        delays: Delays::default(),
    }
}

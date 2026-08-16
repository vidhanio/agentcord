//! Shared test fixtures (test-only module; never compiled into the bot).

use std::{path::PathBuf, time::Duration};

use serenity::all::{GuildId, UserId};

use crate::config::{
    CATCHUP_BACKLOG, CONTROL_REPLY_LIMIT, CONTROL_TIMEOUT, Config, DEFAULT_HARNESS, Delays,
    Discord, HerdrConfig, MAX_SYNC_MESSAGES,
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
        discord: Discord {
            bot_token: "token".into(),
            guild_id: GuildId::new(1),
            allowed_user_id: UserId::new(1),
            control_reply_limit: CONTROL_REPLY_LIMIT,
            max_sync_messages: MAX_SYNC_MESSAGES,
            catchup_backlog: CATCHUP_BACKLOG,
        },
        herdr: HerdrConfig {
            socket_path: None,
            session: None,
            default_harness: DEFAULT_HARNESS,
            control_command: command.map(str::to_owned),
            control_cwd: cwd,
            control_timeout: timeout.unwrap_or(CONTROL_TIMEOUT),
        },
        delays: Delays::default(),
    }
}

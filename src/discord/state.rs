//! Runtime state owned by Discord integrations.

use std::sync::OnceLock;

use serenity::all::{Context, Webhook};

use crate::{BotError, BotResult};

/// Discord handles shared by forum, webhook, command, and event code.
#[derive(Default)]
pub struct State {
    /// Serenity context installed after the gateway becomes ready.
    context: OnceLock<Context>,
    /// Cached webhook used to mirror prompts as the allowed user.
    webhook: tokio::sync::Mutex<Option<Webhook>>,
}

impl State {
    /// Installs the first Serenity context delivered by the gateway.
    pub fn install_context(&self, context: Context) {
        self.context.get_or_init(|| context);
    }

    /// Returns the installed Serenity context.
    pub fn context(&self) -> BotResult<&Context> {
        self.context.get().ok_or(BotError::DiscordNotReady)
    }

    /// Reports whether a Serenity context has been installed.
    pub fn is_ready(&self) -> bool {
        self.context.get().is_some()
    }

    /// Returns the mutex protecting the prompt webhook cache.
    pub const fn webhook(&self) -> &tokio::sync::Mutex<Option<Webhook>> {
        &self.webhook
    }
}

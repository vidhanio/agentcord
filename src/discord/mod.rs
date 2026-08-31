//! Discord-facing state, commands, rendering, and forum integrations.

use std::path::{Path, PathBuf};

use crate::{BotError, BotResult};

/// Expands a leading `~` using the current user's home directory.
pub(crate) fn expand_home(path: &Path) -> BotResult<PathBuf> {
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

/// Replaces an absolute path prefix matching the current home directory with
/// `~`.
pub(crate) fn shorten_home(value: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return value.to_owned();
    };
    let home = home.to_string_lossy();
    let Some(suffix) = value.strip_prefix(home.as_ref()) else {
        return value.to_owned();
    };
    if suffix.is_empty() {
        return "~".to_owned();
    }
    if suffix.starts_with('/') || suffix.starts_with('\\') {
        return format!("~{suffix}");
    }
    value.to_owned()
}

/// Formats a project path relative to the configured base or home directory.
pub(crate) fn project_label(path: &Path, base_path: &Path) -> String {
    if !base_path.as_os_str().is_empty()
        && let Ok(base_path) = expand_home(base_path)
        && let Ok(relative) = path.strip_prefix(base_path)
        && !relative.as_os_str().is_empty()
    {
        return relative.display().to_string();
    }
    shorten_home(&path.display().to_string())
}

pub(crate) mod commands;
pub(crate) mod forum;
pub(crate) mod permission;
pub mod render;
pub(crate) mod state;
pub(crate) mod webhook;

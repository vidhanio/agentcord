//! Discord-facing state, commands, rendering, and forum integrations.

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

pub(crate) mod commands;
pub(crate) mod forum;
pub(crate) mod permission;
pub mod render;
pub(crate) mod state;
pub(crate) mod webhook;

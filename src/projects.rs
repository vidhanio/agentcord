//! Project path resolution used by new ACP sessions.

use std::path::{Path, PathBuf};

use crate::{BotError, BotResult, config::ProjectsConfig};

/// A validated project directory and its short Discord label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    /// Human-readable path label used in a forum title.
    pub label: String,
    /// Canonical absolute working directory sent to ACP.
    pub path: PathBuf,
}

impl Project {
    /// Resolves a user-supplied directory and derives its display label.
    pub fn resolve(config: &ProjectsConfig, input: &str) -> BotResult<Self> {
        let path =
            Self::canonical_directory(&Self::expand_home(Path::new(input))?, "project directory")?;
        let base =
            Self::canonical_directory(&Self::expand_home(&config.base_path)?, "project base")?;
        let home = dirs::home_dir().and_then(|path| path.canonicalize().ok());
        Ok(Self {
            label: Self::display_label(&path, &base, home.as_deref()),
            path,
        })
    }

    /// Derives a display label for a path reported by an imported ACP session.
    ///
    /// Imported sessions may outlive their local checkout, so this function
    /// keeps the reported absolute path when canonicalization is no longer
    /// possible.
    pub fn adopt(config: &ProjectsConfig, reported: &Path) -> Self {
        let path = reported
            .canonicalize()
            .unwrap_or_else(|_| reported.to_owned());
        let base = Self::expand_home(&config.base_path)
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| config.base_path.clone());
        let home = dirs::home_dir().and_then(|path| path.canonicalize().ok());
        Self {
            label: Self::display_label(&path, &base, home.as_deref()),
            path,
        }
    }

    /// Canonicalizes a path and confirms that it is a directory.
    fn canonical_directory(path: &Path, description: &str) -> BotResult<PathBuf> {
        let canonical = path.canonicalize().map_err(|error| {
            BotError::InvalidRequest(format!(
                "could not resolve {description} `{}`: {error}",
                path.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(BotError::InvalidRequest(format!(
                "{description} `{}` is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Expands a leading `~` using the current user's home directory.
    fn expand_home(path: &Path) -> BotResult<PathBuf> {
        let text = path.to_string_lossy();
        if text == "~" || text.starts_with("~/") {
            let home = dirs::home_dir().ok_or_else(|| {
                BotError::InvalidRequest("could not determine home directory".into())
            })?;
            return Ok(if text == "~" {
                home
            } else {
                home.join(&text[2..])
            });
        }
        Ok(path.to_owned())
    }

    /// Chooses a short stable label relative to the configured base or home.
    fn display_label(path: &Path, base: &Path, home: Option<&Path>) -> String {
        if let Ok(relative) = path.strip_prefix(base)
            && !relative.as_os_str().is_empty()
        {
            return relative.to_string_lossy().into_owned();
        }
        if let Some(home) = home
            && let Ok(relative) = path.strip_prefix(home)
        {
            return format!("~/{}", relative.display());
        }
        path.display().to_string()
    }
}

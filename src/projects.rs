use std::path::{Path, PathBuf};

use crate::{BotError, BotResult, config::ProjectsConfig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    pub label: String,
    pub path: PathBuf,
}

pub fn resolve(config: &ProjectsConfig, input: &str) -> BotResult<Project> {
    let path = canonicalize(&expand_home(Path::new(input))?, "project directory")?;
    let display_base = canonicalize(&expand_home(&config.base_path)?, "display base")?;
    let home = dirs::home_dir().and_then(|path| path.canonicalize().ok());
    let label = display_label(&path, &display_base, home.as_deref());
    Ok(Project { label, path })
}

fn canonicalize(path: &Path, description: &str) -> BotResult<PathBuf> {
    let canonical = path.canonicalize().map_err(|error| {
        BotError::Config(format!(
            "could not canonicalize {description} {}: {error}",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(BotError::Config(format!(
            "{description} {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn expand_home(path: &Path) -> BotResult<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| BotError::Config("could not determine the home directory".into()))?;
        return Ok(if text == "~" {
            home
        } else {
            home.join(&text[2..])
        });
    }
    Ok(path.to_owned())
}

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

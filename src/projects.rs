use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{BotError, BotResult, config::ProjectsConfig};

pub const DISCORD_PROJECT_LIMIT: usize = 25;

/// A configured working directory. The canonical directory is its identity;
/// the label is presentation only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    pub label: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProjectCatalog {
    projects: Vec<Project>,
}

impl ProjectCatalog {
    pub fn discover(config: &ProjectsConfig) -> BotResult<Self> {
        let display_base = canonicalize(&expand_home(&config.base_path)?, "display base")?;
        let home = dirs::home_dir().and_then(|path| path.canonicalize().ok());
        let mut by_path = BTreeMap::new();
        for configured in &config.directories {
            let path = canonicalize(&expand_home(configured)?, "project directory")?;
            let label = display_label(&path, &display_base, home.as_deref());
            by_path.insert(path.clone(), Project { label, path });
        }
        let mut projects = by_path.into_values().collect::<Vec<_>>();
        projects.sort_by(|left, right| left.label.cmp(&right.label));
        if projects.len() > DISCORD_PROJECT_LIMIT {
            return Err(BotError::Config(format!(
                "{} configured directories exceed Discord's {DISCORD_PROJECT_LIMIT}-option selector limit",
                projects.len()
            )));
        }
        Ok(Self { projects })
    }

    #[must_use]
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn resolve(&self, selection: &str) -> BotResult<Project> {
        let index = selection
            .parse::<usize>()
            .map_err(|_| BotError::Other("invalid directory selection".into()))?;
        let configured = self
            .projects
            .get(index)
            .ok_or_else(|| BotError::Other("unknown directory selection".into()))?;
        let path = canonicalize(&configured.path, "project directory")?;
        Ok(Project {
            label: configured.label.clone(),
            path,
        })
    }
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

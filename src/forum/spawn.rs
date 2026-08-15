//! Agent spawn helpers: creating a tab or workspace and starting the agent
//! in it (with cleanup on failure), plus the naming and cwd resolution for
//! a fresh launch.

use std::collections::HashSet;

use tracing::{info, warn};

use crate::{
    BotResult,
    forum::Forum,
    herdr::{Agent, Workspace},
    session::Harness,
};

/// The agent-name stamp for every fresh launch: `herdcord-<UTC
/// timestamp>` (`herdcord-20260814-231527`). Numeric, zero-padded, and in
/// UTC so names sort chronologically; two launches in the same second get
/// a numeric suffix from [`Forum::unique_agent_name`].
#[must_use]
fn agent_name_stamp() -> String {
    format!(
        "herdcord-{}",
        jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S")
    )
}

impl Forum {
    /// A unique name for a fresh agent: the [`agent_name_stamp`] with a
    /// numeric suffix when the same second already produced a live agent.
    pub(crate) async fn fresh_agent_name(&self) -> BotResult<String> {
        self.unique_agent_name(&agent_name_stamp()).await
    }

    /// A herdr agent name based on `base` that no live agent uses: `base`
    /// itself, or `base-2`, `base-3`, … when taken.
    pub(crate) async fn unique_agent_name(&self, base: &str) -> BotResult<String> {
        let taken = self
            .herdr
            .list_agents()
            .await?
            .into_iter()
            .filter_map(|agent| agent.name)
            .collect::<HashSet<_>>();
        if !taken.contains(base) {
            return Ok(base.to_owned());
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
            suffix += 1;
        }
    }

    /// The working directory for a new agent in `workspace_label`: the cwd
    /// of a live agent in the workspace when there is one, else the cwd of
    /// a previous session, else the user's home directory.
    pub(crate) async fn launch_cwd(&self, workspace_label: &str) -> String {
        // Agents report their workspace by herdr's positional id; match
        // through the workspace list so the identity is the label, the
        // same one the rows use.
        if let (Ok(agents), Ok(workspaces)) = (
            self.herdr.list_agents().await,
            self.herdr.list_workspaces().await,
        ) && let Some(agent) = agents.iter().find(|agent| {
            workspaces.iter().any(|workspace| {
                workspace.workspace_id == agent.workspace_id && workspace.label == workspace_label
            })
        }) {
            return agent.cwd.to_string_lossy().into_owned();
        }
        if let Ok(sessions) = self.db.sessions_by_workspace(workspace_label).await
            && let Some(session) = sessions.first()
        {
            return session.cwd.clone();
        }
        dirs::home_dir().map_or_else(
            || "/tmp".to_owned(),
            |dir| dir.to_string_lossy().into_owned(),
        )
    }

    /// Spawns a herdr agent in `workspace`: creates a tab and starts the
    /// agent in it under `name`. The tab is closed again if the agent fails
    /// to start.
    pub async fn spawn_in_workspace(
        &self,
        workspace: &Workspace,
        name: &str,
        harness: Harness,
        cwd: &str,
        args: &[String],
    ) -> BotResult<Agent> {
        let tab = match self
            .herdr
            .create_tab(&workspace.workspace_id, name, cwd)
            .await
        {
            Ok(tab) => tab,
            Err(error) => return Err(error.into()),
        };

        match self
            .herdr
            .start_agent(name, harness, &tab.pane_id, args)
            .await
        {
            Ok(agent) => {
                info!(?agent, "started agent");
                Ok(agent)
            }
            Err(error) => {
                if let Err(close_error) = self.herdr.close_tab(&tab.tab_id).await {
                    warn!(
                        ?close_error,
                        "failed to clean up tab after failed agent start"
                    );
                }
                Err(error.into())
            }
        }
    }

    /// Spawns a herdr agent in a fresh workspace created with `label` and
    /// `cwd`, under `name`. The workspace is closed again if the agent
    /// fails to start.
    pub async fn spawn_in_new_workspace(
        &self,
        label: &str,
        name: &str,
        harness: Harness,
        cwd: &str,
        args: &[String],
    ) -> BotResult<Agent> {
        let created = self.herdr.create_workspace_with_pane(label, cwd).await?;

        match self
            .herdr
            .start_agent(name, harness, &created.pane_id, args)
            .await
        {
            Ok(agent) => {
                info!(?agent, "started agent");
                Ok(agent)
            }
            Err(error) => {
                if let Err(close_error) = self
                    .herdr
                    .close_workspace(&created.workspace.workspace_id)
                    .await
                {
                    warn!(
                        ?close_error,
                        "failed to clean up workspace after failed agent start"
                    );
                }
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::agent_name_stamp;

    #[test]
    fn agent_name_stamp_is_lowercase_numeric_and_dated() {
        let name = agent_name_stamp();
        assert_eq!(name.len(), "herdcord-".len() + 15);
        assert!(
            name.starts_with("herdcord-"),
            "name should start with the prefix: {name}"
        );
        let stamp = &name["herdcord-".len()..];
        assert!(
            stamp.chars().all(|ch| ch.is_ascii_digit() || ch == '-'),
            "stamp should be purely numeric: {stamp}"
        );
        // `herdcord-YYYYMMDD-HHMMSS` — two dash-separated numeric groups.
        let mut parts = stamp.split('-');
        assert_eq!(parts.next().map(str::len), Some(8));
        assert_eq!(parts.next().map(str::len), Some(6));
        assert_eq!(parts.next(), None);
    }
}

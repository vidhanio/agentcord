//! Event-driven sync: the long-lived herdr events.subscribe loop, the
//! reconcile drift backstop, and pane lifecycle handling (typing
//! indicators, post inactivation on death).

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use serenity::all::{
    ChannelId, Context, CreateMessage, EditThread, ThreadId, Typing as TypingHandle,
};
use tracing::{info, warn};

use crate::{
    BotResult,
    db::SessionRow,
    forum::{Forum, from_i64, titles::session_intro},
    herdr::{
        Agent, AgentStatus, Event, EventKind, EventStream, PaneId, SessionPath, Subscription,
        WorkspaceId,
    },
};

/// A typing indicator on session posts, owned by the event loop: started
/// when an agent starts working, stopped when the entry is dropped. One
/// task per pane, no shared state.
#[derive(Default)]
struct Typing {
    tasks: HashMap<PaneId, TypingHandle>,
}

impl Typing {
    /// Starts the typing indicator for `post` if one is not already
    /// running for this pane.
    fn start(&mut self, ctx: &Context, pane_id: &PaneId, post: ChannelId) {
        if self.tasks.contains_key(pane_id) {
            return;
        }
        self.tasks.insert(
            pane_id.clone(),
            TypingHandle::start(Arc::clone(&ctx.http), post.widen()),
        );
    }
}

impl Forum {
    /// Marks a session's post inactive: the status tag is dropped (the
    /// harness tag stays), the starter message's pane part flips to the
    /// inactive marker, and the thread is closed — archived, never locked,
    /// so a message still auto-unarchives it and resumes the session. A
    /// live agent's sync reopens it. Idempotent: an already-archived post
    /// (a previous inactivation, or a manual/Discord archive) is only
    /// re-tagged — Discord rejects message edits in archived threads, so
    /// the starter refresh and the close are skipped for it.
    async fn inactivate_post(&self, ctx: &Context, session: &SessionRow) {
        let Some(post_id) = session.post_channel_id else {
            return;
        };
        let Ok(post) = from_i64(post_id) else {
            return;
        };
        if let Ok(forum) = self.forum_for_post(ctx, post).await
            && let Err(error) = self.dead_post_tags(ctx, forum, post).await
        {
            warn!(
                ?error,
                session = %session.session_path,
                "failed to drop status tags of dead session"
            );
        }
        if self.post_archived(ctx, post).await {
            return;
        }
        let intro = session_intro(
            None,
            None,
            Path::new(&session.cwd),
            Some(&SessionPath::from(session.session_path.clone())),
        );
        if let Err(error) = self.refresh_starter(ctx, session, post, intro).await {
            warn!(
                ?error,
                session = %session.session_path,
                "failed to mark session starter inactive"
            );
        }
        // The post is closed while the session is dead. Archived, never
        // locked: a message still auto-unarchives the thread and resumes
        // the session. The active path reopens it.
        if let Err(error) = ThreadId::new(post.get())
            .edit(&ctx.http, EditThread::new().archived(true))
            .await
        {
            warn!(
                ?error,
                session = %session.session_path,
                "failed to close inactive session post"
            );
        }
    }

    /// Whether `post` is an archived thread. Unknown state (deleted or
    /// unwrapped channel) counts as not archived, so the caller attempts
    /// its writes and surfaces the real error.
    async fn post_archived(&self, ctx: &Context, post: ChannelId) -> bool {
        self.forum_thread(ctx, post)
            .await
            .is_ok_and(|thread| thread.thread_metadata.archived())
    }

    /// Reconciles the forums with herdr: ensures (and renames) a forum per
    /// workspace, ensures a post per agent session and applies its metadata
    /// (tags, title, unarchive), drops the
    /// status tags and closes the posts of sessions with no live agent
    /// (the harness tag stays; a message still unarchives the thread and
    /// resumes the session), prunes stale
    /// workspace/session rows whose Discord channels are gone, and prunes
    /// the pane→session map of panes herdr no longer reports. herdr is the
    /// source of truth for live state; the database holds the bindings and
    /// mirror cursors. The transcript mirror is not this pass's job — the
    /// 2s poll owns it.
    async fn reconcile(&self, ctx: &Context, typing: &mut Typing) -> BotResult<()> {
        let workspaces = self.herdr.list_workspaces().await?;

        // Re-key rows stored under the legacy positional-id identity.
        if let Err(error) = self.db.migrate_workspace_ids(&workspaces).await {
            warn!(?error, "failed to re-key workspace rows to labels");
        }
        if let Err(error) = self.db.migrate_session_labels(&workspaces).await {
            warn!(?error, "failed to re-key session rows to labels");
        }

        for workspace in &workspaces {
            if let Err(error) = self.sync_workspace_forum(ctx, workspace).await {
                warn!(?error, workspace = %workspace.label, "failed to sync workspace forum");
            }
        }

        self.prune_stale_workspaces(ctx, &workspaces).await;

        let agents = self.herdr.list_agents().await?;

        let mut live_paths = HashSet::new();
        let mut live_panes = HashSet::new();
        for agent in &agents {
            if let Some(path) = agent
                .agent_session
                .as_ref()
                .map(|session| session.value.clone())
            {
                live_paths.insert(path);
            }
            live_panes.insert(agent.pane_id.clone());

            self.sync_agent_typing(typing, ctx, agent).await;
            self.sync_agent_post(ctx, agent).await;
        }

        // Panes herdr no longer reports (a missed pane.closed event, e.g.
        // herdr restarted between connects) are dropped from the live-session
        // set so the poll stops watching them.
        self.sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .retain(|pane, _| live_panes.contains(pane));

        // Sessions whose agent is gone get their post inactivated (status
        // tag dropped, starter marked inactive, post closed), and their
        // tool-embed bookkeeping is dropped. A session is live when any
        // agent's session value matches its key or adopted transcript. A
        // dead session whose post was deleted too is stale: the row is
        // pruned instead of inactivated, so it stops producing 404 noise.
        self.prune_stale_sessions(ctx, &live_paths).await;

        Ok(())
    }

    /// Deletes session rows whose post was deleted and whose agent is gone:
    /// the thread is unrecoverable, so the row is stale state. Dead
    /// sessions with a live post are inactivated instead (status tag
    /// dropped, starter marked inactive, post closed).
    async fn prune_stale_sessions(&self, ctx: &Context, live_paths: &HashSet<SessionPath>) {
        for session in self
            .db
            .all_sessions()
            .await
            .inspect_err(|error| warn!(?error, "failed to list sessions for pruning"))
            .unwrap_or_default()
        {
            let Some(post_id) = session.post_channel_id else {
                continue;
            };
            if live_paths.iter().any(|path| session.hosts(path.as_str())) {
                continue;
            }
            self.drop_session_bookkeeping(&session.session_path);
            let Ok(post) = from_i64(post_id) else {
                continue;
            };
            match self.channel_exists(ctx, post).await {
                Ok(false) => {
                    info!(
                        session = %session.session_path,
                        ?post,
                        "pruning stale session row (post deleted)"
                    );
                    if let Err(error) = self.db.delete_session(&session.session_path).await {
                        warn!(
                            session = %session.session_path,
                            ?error,
                            "failed to prune stale session row"
                        );
                    }
                }
                // Transient failure: fall back to the inactivation path,
                // which swallows its own errors.
                Err(error) => warn!(
                    ?error,
                    session = %session.session_path,
                    "failed to check session post existence"
                ),
                Ok(true) => self.inactivate_post(ctx, &session).await,
            }
        }
    }

    /// Runs the forum's event loop: bootstraps the pane→agent cache from a
    /// session snapshot, runs one startup reconcile, subscribes to herdr
    /// pane/workspace events, and keeps session posts in sync both on events
    /// and on a periodic reconcile. The subscription is re-established (and
    /// the cache re-seeded) whenever the event stream dies or the agent set
    /// changes.
    pub async fn run_event_loop(&self, ctx: Context) {
        // Typing indicators are owned here, in the event loop: started on a
        // working status, stopped on any settled status or pane close.
        let mut typing = Typing::default();

        loop {
            // Reconnect loop: (re)establish the subscription, catch up on
            // anything missed while disconnected, then serve events until
            // the stream dies or a new agent requires re-subscribing.
            let mut stream = self.connect().await;

            if let Err(error) = self.reconcile(&ctx, &mut typing).await {
                warn!(?error, "startup forum reconcile failed");
            }

            let mut tick = tokio::time::interval(self.config.delays.sync_interval);

            loop {
                tokio::select! {
                    event = stream.recv() => {
                        if let Some(event) = event {
                            if self.handle_event(&ctx, event, &mut typing).await {
                                // The agent set changed; re-subscribe so
                                // status events cover the new agent's pane.
                                warn!("agent set changed, re-subscribing");
                                break;
                            }
                        } else {
                            warn!("herdr event subscription ended, reconnecting");
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        if let Err(error) = self.reconcile(&ctx, &mut typing).await {
                            warn!(?error, "forum reconcile failed");
                        }
                    }
                }
            }
        }
    }

    /// Subscribes to herdr events, retrying until the subscription is
    /// established. The subscription set is rebuilt from a fresh session
    /// snapshot on every (re)connect.
    async fn connect(&self) -> EventStream {
        loop {
            let agents = match self.herdr.session_snapshot().await {
                Ok(agents) => agents,
                Err(error) => {
                    warn!(?error, "failed to snapshot herdr session, retrying");
                    tokio::time::sleep(self.config.delays.resubscribe_delay).await;
                    continue;
                }
            };

            match self.herdr.subscribe(&Self::subscriptions(&agents)).await {
                Ok(stream) => return stream,
                Err(error) => {
                    warn!(?error, "failed to subscribe to herdr events, retrying");
                    tokio::time::sleep(self.config.delays.resubscribe_delay).await;
                }
            }
        }
    }

    /// The subscription set for `agents`: session-wide lifecycle events
    /// plus a pane-scoped status subscription for every agent pane.
    fn subscriptions(agents: &[Agent]) -> Vec<Subscription> {
        let mut subscriptions = vec![
            Subscription::new(EventKind::WorkspaceCreated),
            Subscription::new(EventKind::WorkspaceUpdated),
            Subscription::new(EventKind::WorkspaceRenamed),
            Subscription::new(EventKind::WorkspaceClosed),
            Subscription::new(EventKind::PaneAgentDetected),
            Subscription::new(EventKind::PaneClosed),
            Subscription::new(EventKind::PaneExited),
        ];

        for agent in agents {
            subscriptions.push(Subscription::for_pane(
                EventKind::PaneAgentStatusChanged,
                agent.pane_id.clone(),
            ));
        }

        subscriptions
    }

    /// Applies one herdr event, returning whether the subscription set must
    /// be rebuilt (a new agent appeared).
    async fn handle_event(&self, ctx: &Context, event: Event, typing: &mut Typing) -> bool {
        match event.kind() {
            Some(EventKind::PaneAgentStatusChanged) => {
                let Some(pane_id) = event.pane_id() else {
                    return false;
                };

                // The event carries only the new status; fetch the agent
                // fresh so the post's title/harness reflect the current state.
                let mut agent = match self.herdr.get_agent(&pane_id).await {
                    Ok(agent) => agent,
                    Err(error) => {
                        warn!(
                            ?error,
                            %pane_id,
                            "failed to fetch agent for status change"
                        );
                        return false;
                    }
                };

                if let Some(status) = event
                    .data
                    .get("agent_status")
                    .and_then(serde_json::Value::as_str)
                {
                    agent.agent_status = status.to_owned();
                }

                self.sync_agent_typing(typing, ctx, &agent).await;
                self.sync_agent_post(ctx, &agent).await;
                // A blocked agent is waiting for input; the user should see
                // that without having to poke it. Posted on every blocked
                // status event — the event fires on the transition into
                // blocked, so a repeated notice means a genuine re-block,
                // and no dedupe window hides one.
                if agent.status() == AgentStatus::Blocked
                    && let Err(error) = self.post_blocked_notice(ctx, &agent).await
                {
                    warn!(?error, %pane_id, "failed to post blocked notice");
                }
                false
            }
            Some(EventKind::PaneAgentDetected) => {
                let Some(pane_id) = event.pane_id() else {
                    return false;
                };

                // Known pane (already mapped to a session): nothing new.
                if self
                    .sessions_by_pane
                    .lock()
                    .expect("sessions_by_pane lock poisoned")
                    .contains_key(&pane_id)
                {
                    return false;
                }

                // A genuinely new agent: re-subscribe so its status events
                // are covered. The reconnect runs a reconcile, which
                // applies the new agent's post metadata; the poll mirrors
                // its transcript. herdr emits `agent_detected` once per
                // label change, so a phantom pane costs one reconnect
                // cycle, not a storm.
                true
            }
            Some(EventKind::PaneClosed | EventKind::PaneExited) => {
                let Some(pane_id) = event.pane_id() else {
                    return false;
                };
                self.mark_pane_closed(ctx, &pane_id, typing).await;
                false
            }
            // A closing workspace kills its agents' panes without per-pane
            // events, so the workspace's sessions are inactivated directly.
            Some(EventKind::WorkspaceClosed) => {
                let Some(workspace_id) = event.workspace_id() else {
                    return false;
                };
                let Ok(Some(workspace)) = self.workspace_by_id(&workspace_id).await else {
                    warn!(%workspace_id, "closed workspace no longer exists");
                    return false;
                };
                match self.db.sessions_by_workspace(&workspace.label).await {
                    Ok(sessions) => {
                        for session in sessions {
                            self.inactivate_post(ctx, &session).await;
                        }
                    }
                    Err(error) => warn!(
                        ?error,
                        workspace = %workspace.label,
                        "failed to list sessions of closed workspace"
                    ),
                }
                false
            }
            // A created workspace gets its forum right away; updated and
            // renamed re-sync the forum to the current label.
            Some(
                EventKind::WorkspaceCreated
                | EventKind::WorkspaceUpdated
                | EventKind::WorkspaceRenamed,
            ) => {
                let Some(workspace_id) = event.workspace_id() else {
                    return false;
                };
                self.sync_workspace_from_event(ctx, &workspace_id).await;
                false
            }
            Some(_) | None => false,
        }
    }

    /// Re-syncs the forum for a workspace event (updated/renamed): the event
    /// carries no label, so the workspace is re-fetched and the forum
    /// channel renamed when the label changed.
    async fn sync_workspace_from_event(&self, ctx: &Context, workspace_id: &WorkspaceId) {
        match self.workspace_by_id(workspace_id).await {
            Ok(Some(workspace)) => {
                if let Err(error) = self.sync_workspace_forum(ctx, &workspace).await {
                    warn!(?error, %workspace_id, "failed to sync workspace forum");
                }
            }
            Ok(None) => {
                warn!(%workspace_id, "workspace update for unknown workspace");
            }
            Err(error) => {
                warn!(?error, %workspace_id, "failed to fetch updated workspace");
            }
        }
    }

    /// Posts "the agent is **blocked**" into the session's post: a stuck
    /// agent is visible without anyone poking it. No dedupe window — each
    /// blocked status event is a genuine state report.
    async fn post_blocked_notice(&self, ctx: &Context, agent: &Agent) -> BotResult<()> {
        let Some(session) = self.session_for_agent(agent).await else {
            return Ok(());
        };
        let Some(post_id) = session.post_channel_id else {
            return Ok(());
        };
        let post = from_i64(post_id)?;
        post.widen()
            .send_message(
                &ctx.http,
                CreateMessage::new().content("the agent is **blocked** — it's waiting for input."),
            )
            .await?;
        Ok(())
    }

    /// Starts or stops the session post's typing indicator to match the
    /// agent's state: working shows it, any settled state drops it (the
    /// task aborts on drop).
    async fn sync_agent_typing(&self, typing: &mut Typing, ctx: &Context, agent: &Agent) {
        let Some(session) = self.session_for_agent(agent).await else {
            return;
        };
        let Some(post_id) = session.post_channel_id else {
            return;
        };
        let Ok(post) = from_i64(post_id) else {
            return;
        };
        if agent.status() == AgentStatus::Working {
            typing.start(ctx, &agent.pane_id, post);
        } else {
            typing.tasks.remove(&agent.pane_id);
        }
    }

    /// Marks a session's post dead when its pane closed, unless another
    /// live pane still hosts the same session (two panes can share one).
    async fn mark_pane_closed(&self, ctx: &Context, pane_id: &PaneId, typing: &mut Typing) {
        let Some(session_path) = self
            .sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .remove(pane_id)
        else {
            return;
        };
        typing.tasks.remove(pane_id);
        let shared = self
            .sessions_by_pane
            .lock()
            .expect("sessions_by_pane lock poisoned")
            .values()
            .any(|path| path.as_str() == session_path.as_str());
        if shared {
            return;
        }
        let Ok(Some(session)) = self.db.get_session(&session_path).await else {
            return;
        };
        // The session is dead: its in-memory bookkeeping goes with it.
        self.drop_session_bookkeeping(&session_path);
        self.inactivate_post(ctx, &session).await;
    }
}

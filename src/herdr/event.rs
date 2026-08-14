//! Event subscription machinery: wire kinds, subscriptions, the event
//! payloads, and the live event stream.

use serde::Deserialize;
use serde_json::Value;
use tokio::{sync::broadcast, task::JoinHandle};

use super::{PaneId, WorkspaceId};

/// The kind of an event received from a herdr subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A workspace was created.
    WorkspaceCreated,
    /// A workspace was updated.
    WorkspaceUpdated,
    /// A workspace was closed.
    WorkspaceClosed,
    /// A workspace was renamed.
    WorkspaceRenamed,
    /// A tab was created.
    TabCreated,
    /// A tab was closed.
    TabClosed,
    /// A pane was created.
    PaneCreated,
    /// A pane was closed.
    PaneClosed,
    /// A pane was updated (output, agent status, etc.).
    PaneUpdated,
    /// A pane exited.
    PaneExited,
    /// An agent was detected in a pane.
    PaneAgentDetected,
    /// An agent's lifecycle status changed. Pane-scoped: subscribing without
    /// a pane id is rejected by the server.
    PaneAgentStatusChanged,
}

impl EventKind {
    /// Returns the dotted subscription name for this kind, as sent in an
    /// `events.subscribe` request.
    #[must_use]
    pub const fn subscription(self) -> &'static str {
        match self {
            Self::WorkspaceCreated => "workspace.created",
            Self::WorkspaceUpdated => "workspace.updated",
            Self::WorkspaceClosed => "workspace.closed",
            Self::WorkspaceRenamed => "workspace.renamed",
            Self::TabCreated => "tab.created",
            Self::TabClosed => "tab.closed",
            Self::PaneCreated => "pane.created",
            Self::PaneClosed => "pane.closed",
            Self::PaneUpdated => "pane.updated",
            Self::PaneExited => "pane.exited",
            Self::PaneAgentDetected => "pane.agent_detected",
            Self::PaneAgentStatusChanged => "pane.agent_status_changed",
        }
    }

    /// Whether this kind must be subscribed with a pane id.
    #[must_use]
    pub const fn requires_pane_scope(self) -> bool {
        matches!(self, Self::PaneAgentStatusChanged)
    }

    /// Maps an underscored wire event name (as received on the socket) to its
    /// kind; unknown names return `None`.
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        Some(match wire {
            "workspace_created" => Self::WorkspaceCreated,
            "workspace_updated" => Self::WorkspaceUpdated,
            "workspace_closed" => Self::WorkspaceClosed,
            "workspace_renamed" => Self::WorkspaceRenamed,
            "tab_created" => Self::TabCreated,
            "tab_closed" => Self::TabClosed,
            "pane_created" => Self::PaneCreated,
            "pane_closed" => Self::PaneClosed,
            "pane_updated" => Self::PaneUpdated,
            "pane_exited" => Self::PaneExited,
            "pane_agent_detected" => Self::PaneAgentDetected,
            // Herdr serializes this one event with its dotted subscription
            // name, unlike every other kind (underscored).
            "pane_agent_status_changed" | "pane.agent_status_changed" => {
                Self::PaneAgentStatusChanged
            }
            _ => return None,
        })
    }
}

/// One entry of an `events.subscribe` request.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub kind: EventKind,
    pub pane_id: Option<PaneId>,
}

impl Subscription {
    /// A session-wide subscription to `kind`.
    #[must_use]
    pub const fn new(kind: EventKind) -> Self {
        Self {
            kind,
            pane_id: None,
        }
    }

    /// A pane-scoped subscription: only events for `pane_id` are delivered.
    #[must_use]
    pub fn for_pane(kind: EventKind, pane_id: impl Into<PaneId>) -> Self {
        Self {
            kind,
            pane_id: Some(pane_id.into()),
        }
    }
}

/// An event received from a herdr subscription.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    /// Underscored wire name of the event, e.g. `"pane_updated"`.
    pub event: String,
    /// The event payload.
    pub data: Value,
}

impl Event {
    /// The kind of this event, when the wire name is known.
    #[must_use]
    pub fn kind(&self) -> Option<EventKind> {
        EventKind::from_wire(&self.event)
    }

    /// The id of the pane this event concerns, when it has one.
    #[must_use]
    pub fn pane_id(&self) -> Option<PaneId> {
        self.data
            .get("pane_id")
            .and_then(Value::as_str)
            .or_else(|| {
                self.data
                    .get("pane")
                    .and_then(|pane| pane.get("pane_id"))
                    .and_then(Value::as_str)
            })
            .map(PaneId::from)
    }

    /// The id of the workspace this event concerns, when it has one.
    #[must_use]
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.data
            .get("workspace_id")
            .and_then(Value::as_str)
            .or_else(|| {
                self.data
                    .get("workspace")
                    .and_then(|workspace| workspace.get("workspace_id"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                self.data
                    .get("pane")
                    .and_then(|pane| pane.get("workspace_id"))
                    .and_then(Value::as_str)
            })
            .map(WorkspaceId::from)
    }
}

/// A live stream of herdr events from an `events.subscribe` connection.
///
/// The connection is driven by a background reader task that forwards event
/// lines onto an internal broadcast channel (capacity 128, drop-oldest on
/// overflow). When the connection dies the task exits and
/// [`EventStream::recv`] returns `None`; callers should re-subscribe.
#[derive(Debug)]
pub struct EventStream {
    receiver: broadcast::Receiver<Event>,
    /// Held to keep the reader task alive; dropped with the stream.
    _reader: JoinHandle<()>,
}

impl EventStream {
    /// Wraps a broadcast receiver and the reader task feeding it.
    #[must_use]
    pub const fn new(receiver: broadcast::Receiver<Event>, reader: JoinHandle<()>) -> Self {
        Self {
            receiver,
            _reader: reader,
        }
    }

    /// Receives the next event, or `None` when the subscription connection
    /// died.
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Wire shape of a single event line on a subscription connection.
#[derive(Debug, Deserialize)]
pub struct EventLine {
    /// Underscored event name, e.g. `"pane_updated"`.
    pub event: String,
    /// Event payload.
    pub data: Value,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn event_kind_subscription_names_round_trip() {
        for (wire, subscription) in [
            ("workspace_created", "workspace.created"),
            ("workspace_updated", "workspace.updated"),
            ("workspace_closed", "workspace.closed"),
            ("workspace_renamed", "workspace.renamed"),
            ("tab_created", "tab.created"),
            ("tab_closed", "tab.closed"),
            ("pane_created", "pane.created"),
            ("pane_closed", "pane.closed"),
            ("pane_updated", "pane.updated"),
            ("pane_exited", "pane.exited"),
            ("pane_agent_detected", "pane.agent_detected"),
        ] {
            let kind = EventKind::from_wire(wire).expect("known wire name");
            assert_eq!(kind.subscription(), subscription);
        }
        assert_eq!(EventKind::from_wire("bogus_event"), None);
    }

    #[test]
    fn event_accessors_read_pane_payloads() {
        let event: Event = serde_json::from_value(json!({
            "event": "pane_updated",
            "data": {
                "pane": {
                    "pane_id": "w4:p1",
                    "workspace_id": "w4",
                    "tab_id": "w4:t1",
                    "agent": "omp",
                    "agent_status": "working",
                    "revision": 42,
                }
            }
        }))
        .unwrap();
        assert_eq!(event.kind(), Some(EventKind::PaneUpdated));
        assert_eq!(
            event.pane_id().as_ref().map(|id| id.as_str()),
            Some("w4:p1")
        );
        assert_eq!(
            event.workspace_id().as_ref().map(|id| id.as_str()),
            Some("w4")
        );

        let closed: Event = serde_json::from_value(json!({
            "event": "workspace_closed",
            "data": { "type": "workspace_closed", "workspace": null, "workspace_id": "w9" },
        }))
        .unwrap();
        assert_eq!(closed.kind(), Some(EventKind::WorkspaceClosed));
        assert_eq!(
            closed.workspace_id().as_ref().map(|id| id.as_str()),
            Some("w9")
        );
    }
}

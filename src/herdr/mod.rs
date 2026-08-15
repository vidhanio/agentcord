//! Typed async client for the herdr Unix-socket JSON API.
//!
//! Each public method maps one herdr API method to a typed result, sends a
//! single newline-delimited JSON request over the herdr Unix socket, and
//! surfaces failures as [`Error`](enum@Error). The data model lives in
//! `model`, the client in `client`, the event-subscription machinery in
//! `event`, and the wire payload types in `wire`.

mod client;
mod error;
mod event;
mod model;
mod wire;

pub use self::{
    client::Herdr,
    error::Error,
    event::{Event, EventKind, EventLine, EventStream, Subscription},
    model::{
        Agent, AgentSession, AgentStatus, CreatedTab, CreatedWorkspace, PaneId, SessionPath,
        Snapshot, TabId, Workspace, WorkspaceId, WorktreeSpace,
    },
};

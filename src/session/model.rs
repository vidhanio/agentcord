//! The normalized conversation model shared by every harness parser.

use std::fmt::{self, Display, Formatter};

use nutype::nutype;

use super::common::{TOOL_TEXT_LIMIT, cap};

/// One side of a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionRole {
    /// The human user.
    User,
    /// The agent harness.
    Agent,
    /// A tool call (name + arguments), not a conversation turn.
    Tool,
}

/// Lifecycle state of a tool call: running until its completion record
/// appears, then done or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolState {
    /// No completion record for the call yet.
    Running,
    /// The call completed without an error.
    Done,
    /// The call's completion record reports an error.
    Failed,
}

impl Display for ToolState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        })
    }
}

/// A tool call id as recorded in a transcript, e.g. `"call_0"` or
/// `"tu_1"`; pairs a call with its completion record and its posted embed.
#[nutype(
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Display,
        Deref,
        Default,
        From,
        Serialize,
        Deserialize
    ),
    default = ""
)]
pub struct ToolCallId(String);

/// A tool call as recorded in the transcript: the call (name + arguments)
/// paired with its completion record when present.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCall {
    /// The harness's call id, used to pair the call with its completion
    /// and to find the posted embed for in-place edits.
    pub call_id: ToolCallId,
    /// The tool's name.
    pub name: String,
    /// Compact-JSON arguments (full — display caps them); `None` when the
    /// call took no arguments.
    pub args: Option<String>,
    /// Whether the call is still running or has completed, computed from
    /// the transcript's completion records.
    pub state: ToolState,
    /// The tool's error text on failure, capped at [`TOOL_TEXT_LIMIT`]
    /// characters.
    pub error: Option<String>,
}

impl Display for ToolCall {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let full = self
            .args
            .as_ref()
            .map_or_else(|| self.name.clone(), |args| format!("{} {args}", self.name));
        f.write_str(&cap(&full, TOOL_TEXT_LIMIT))
    }
}

/// A normalized conversation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMessage {
    /// Which side of the conversation produced this message.
    pub role: SessionRole,
    /// The message text.
    pub text: String,
    /// Structured tool-call data when `role` is Tool.
    pub tool: Option<ToolCall>,
}

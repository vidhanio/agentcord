//! Errors from talking to the herdr socket.

use thiserror::Error;

/// Errors from talking to the herdr socket.
#[derive(Debug, Error)]
pub enum Error {
    /// The herdr server reported a failure.
    #[error("herdr server error {code}: {message}")]
    Herdr {
        /// Machine-readable error code, e.g. `agent_not_found` or `timeout`.
        code: String,
        /// Human-readable error message.
        message: String,
    },
    /// The request exceeded the operation timeout.
    #[error("herdr request timed out")]
    Timeout,
    /// The herdr socket could not be reached or read.
    #[error("failed to communicate with herdr socket: {0}")]
    Io(#[from] std::io::Error),
    /// The response could not be parsed as the expected shape.
    #[error("failed to parse herdr response: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Returns `true` when the operation exceeded its allowed time: either the
    /// local operation timeout or a `timeout` error code from the herdr server
    /// (e.g. `agent prompt` still working after its wait deadline).
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Herdr { code, .. } => code == "timeout",
            Self::Io(_) | Self::Json(_) => false,
        }
    }

    /// Whether the error is herdr's "prompt produced no observed state
    /// change" stall: the prompt was delivered, the agent just did not
    /// transition within herdr's short window. Callers keep waiting and
    /// let the transcript sync surface the message instead of failing.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        matches!(self, Self::Herdr { code, .. } if code == "agent_prompt_stalled")
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn timeout_errors_report_is_timeout() {
        assert!(Error::Timeout.is_timeout());
        assert!(
            Error::Herdr {
                code: "timeout".into(),
                message: "still working".into(),
            }
            .is_timeout()
        );
        assert!(
            !Error::Herdr {
                code: "agent_not_found".into(),
                message: "no such agent".into(),
            }
            .is_timeout()
        );
    }
}

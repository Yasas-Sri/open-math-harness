//! Structured errors shared by every interface.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a request did not produce a result. Callers branch on the code; the
/// message is diagnostic text and must never contain handles or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was malformed or violated a protocol limit.
    InvalidInput,
    /// The sender lacks authority. Nothing was dispatched.
    PermissionDenied,
    /// The handle number names nothing in the sender's table.
    UnknownHandle,
    /// The handle refers to an endpoint generation that has been replaced or
    /// has gone away. It is never retargeted to the new provider.
    StaleHandle,
    /// The capability behind the handle is gone: expired, closed, or withdrawn
    /// while the call was queued.
    Revoked,
    /// No provider can take the request.
    Unavailable,
    /// The endpoint's bounded queue is full. Nothing was dispatched.
    QueueFull,
    /// A per-process limit (outstanding requests, handles, processes) is reached.
    LimitExceeded,
    /// The capability's shared call budget is spent.
    BudgetExhausted,
    /// The caller cancelled before an outcome was observed.
    Cancelled,
    /// The deadline passed before dispatch, or during an observation.
    Timeout,
    /// A mutation was dispatched and its outcome was not observed. This is not
    /// evidence of failure; do not retry without an idempotency mechanism.
    UnknownOutcome,
    /// The kernel is shutting down and admits no new work.
    ShuttingDown,
    /// The provider reported a failure of its own.
    ServiceError,
}

/// An [`ErrorCode`] with diagnostic text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    /// Builds an error.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

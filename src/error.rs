use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Internal,
    InvalidRequest,
    UnsupportedVersion,
    NotFound,
    IdempotencyConflict,
    StaleRevision,
    StaleControlRevision,
    AttachmentRejected,
    ControlDenied,
}

#[derive(Debug, Error)]
pub enum TyrionError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("protocol version {actual} is unsupported; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
    #[error("commission {0} was not found")]
    NotFound(String),
    #[error("the idempotency key was already used for a different request")]
    IdempotencyConflict,
    #[error("stale commission revision: expected {expected}, current revision is {actual}")]
    StaleRevision { expected: i64, actual: i64 },
    #[error(
        "stale attachment control revision: expected {expected}, current revision is {actual}"
    )]
    StaleControlRevision { expected: i64, actual: i64 },
    #[error("attachment rejected: {0}")]
    AttachmentRejected(String),
    #[error("control denied: {0}")]
    ControlDenied(String),
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("database failure: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization failure: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl TyrionError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::UnsupportedVersion { .. } => ErrorCode::UnsupportedVersion,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::IdempotencyConflict => ErrorCode::IdempotencyConflict,
            Self::StaleRevision { .. } => ErrorCode::StaleRevision,
            Self::StaleControlRevision { .. } => ErrorCode::StaleControlRevision,
            Self::AttachmentRejected(_) => ErrorCode::AttachmentRejected,
            Self::ControlDenied(_) => ErrorCode::ControlDenied,
            Self::Io(_) | Self::Database(_) | Self::Serialization(_) => ErrorCode::Internal,
        }
    }

    pub fn details(&self) -> Option<Value> {
        match self {
            Self::StaleRevision { expected, actual } => Some(serde_json::json!({
                "expected_revision": expected,
                "current_revision": actual,
            })),
            Self::StaleControlRevision { expected, actual } => Some(serde_json::json!({
                "expected_control_revision": expected,
                "current_control_revision": actual,
            })),
            _ => None,
        }
    }
}

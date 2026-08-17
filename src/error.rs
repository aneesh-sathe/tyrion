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
            Self::Io(_) | Self::Database(_) | Self::Serialization(_) => ErrorCode::Internal,
        }
    }

    pub fn details(&self) -> Option<Value> {
        match self {
            Self::StaleRevision { expected, actual } => Some(serde_json::json!({
                "expected_revision": expected,
                "current_revision": actual,
            })),
            _ => None,
        }
    }
}

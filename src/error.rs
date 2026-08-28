use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationFailureKind {
    StaleBase,
    Conflict,
}

impl IntegrationFailureKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StaleBase => "stale_base",
            Self::Conflict => "conflict",
        }
    }
}

impl std::fmt::Display for IntegrationFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

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
    #[error("attachment rejected: {message}")]
    AttachmentRejectedWithDetails { message: String, details: Value },
    #[error("control denied: {0}")]
    ControlDenied(String),
    #[error("Worker Lease expired {operation}")]
    WorkerLeaseExpired { operation: &'static str },
    #[error("Worker was interrupted by the Principal")]
    WorkerInterrupted,
    #[error("Watchdog contained the Attempt after detecting {signal}")]
    WatchdogContained { signal: &'static str },
    #[error("Worker Configuration {configuration_id} is unavailable: {message}")]
    WorkerConfigurationUnavailable {
        configuration_id: String,
        message: String,
    },
    #[error(
        "Worker Configuration {configuration_id} could not execute Required Skill Version {skill_name}@{content_digest}: {message}"
    )]
    RequiredSkillUnavailable {
        configuration_id: String,
        skill_name: String,
        content_digest: String,
        message: String,
    },
    #[error(
        "max_storage_bytes exceeded: Git artifacts require at least {required_bytes} bytes; start a new Commission with max_storage_bytes of {required_bytes} or more (current ceiling: {ceiling_bytes})."
    )]
    StorageCeilingExceeded {
        required_bytes: u64,
        ceiling_bytes: u64,
    },
    #[error("Integration {kind}: {message}")]
    IntegrationFailure {
        kind: IntegrationFailureKind,
        message: String,
    },
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
            Self::AttachmentRejected(_) | Self::AttachmentRejectedWithDetails { .. } => {
                ErrorCode::AttachmentRejected
            }
            Self::ControlDenied(_) => ErrorCode::ControlDenied,
            Self::WorkerLeaseExpired { .. }
            | Self::WorkerInterrupted
            | Self::WatchdogContained { .. }
            | Self::WorkerConfigurationUnavailable { .. }
            | Self::RequiredSkillUnavailable { .. }
            | Self::StorageCeilingExceeded { .. }
            | Self::IntegrationFailure { .. } => ErrorCode::InvalidRequest,
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
            Self::AttachmentRejectedWithDetails { details, .. } => Some(details.clone()),
            _ => None,
        }
    }
}

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ErrorCode, TyrionError};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommissionProposal {
    pub goal: String,
    #[serde(default)]
    pub execution: ExecutionSpec,
    pub criteria: Vec<AcceptanceCriterion>,
    pub authority: AuthorityEnvelope,
    pub resource_ceilings: ResourceCeilings,
    #[serde(default)]
    pub known_uncertainties: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    pub verifier: Verifier,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verifier {
    ExactMatch { expected: String },
    Command { argv: Vec<String> },
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionSpec {
    #[default]
    Deterministic,
    CodexGit {
        repository: String,
        base_revision: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthorityEnvelope {
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub destinations: Vec<String>,
    #[serde(default)]
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceCeilings {
    pub max_attempts: u32,
    pub max_elapsed_seconds: u64,
    pub max_worker_concurrency: u32,
    pub max_storage_bytes: u64,
    pub max_model_spend_cents: u64,
    pub max_paid_service_spend_cents: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    pub protocol_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_token: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub expected_revision: Option<i64>,
    #[serde(default)]
    pub expected_control_revision: Option<i64>,
    pub command: Command,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdapterIdentity {
    pub harness: String,
    pub adapter_identity: String,
    pub adapter_version: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttachmentHandshake {
    pub adapter: AdapterIdentity,
    pub adapter_protocol_version: u16,
    pub native_session_id: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommissionReplayCursor {
    pub commission_id: String,
    pub last_event_sequence: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    CreateProposal {
        proposal: Box<CommissionProposal>,
    },
    InspectCommission {
        commission_id: String,
    },
    AcceptCommission {
        commission_id: String,
    },
    IssueAttachmentToken {
        expected_adapter: AdapterIdentity,
        ttl_seconds: u64,
    },
    ConnectAttachment {
        launch_token: String,
        handshake: Box<AttachmentHandshake>,
        replay: Option<CommissionReplayCursor>,
    },
    ResumeAttachment {
        handshake: Box<AttachmentHandshake>,
        replay: CommissionReplayCursor,
    },
    TakeControl {
        commission_id: String,
    },
    ReplayEvents {
        commission_id: String,
        after_sequence: i64,
    },
}

impl Command {
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::InspectCommission { .. }
                | Self::ResumeAttachment { .. }
                | Self::ReplayEvents { .. }
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub protocol_version: u16,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    pub fn success(data: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(error: &TyrionError) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            data: None,
            error: Some(ResponseError {
                code: error.code(),
                message: error.to_string(),
                details: error.details(),
            }),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

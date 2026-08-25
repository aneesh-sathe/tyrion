use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ErrorCode, TyrionError};

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommissionProposal {
    pub goal: String,
    #[serde(default)]
    pub execution: ExecutionSpec,
    #[serde(default)]
    pub worker_requirements: WorkerRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<CommissionPlan>,
    pub criteria: Vec<AcceptanceCriterion>,
    pub authority: AuthorityEnvelope,
    pub resource_ceilings: ResourceCeilings,
    #[serde(default)]
    pub known_uncertainties: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommissionPlan {
    pub assignments: Vec<PlannedAssignment>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedAssignment {
    pub id: String,
    pub goal: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub criterion_ids: Vec<String>,
    pub purpose: AssignmentPurpose,
    #[serde(default)]
    pub read_scopes: Vec<String>,
    #[serde(default)]
    pub write_scopes: Vec<String>,
    pub resources: AssignmentResources,
    #[serde(default)]
    pub worker_requirements: WorkerRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub competition: Option<CompetitionPlan>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequirements {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<SkillVersion>,
    #[serde(default)]
    pub selected_skills: Vec<SelectedSkillVersion>,
    #[serde(default)]
    pub min_context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_strategy: Option<String>,
    #[serde(default)]
    pub assignment_constraints: Vec<String>,
    #[serde(default)]
    pub require_configurations: Vec<String>,
    #[serde(default)]
    pub exclude_configurations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct SkillVersion {
    pub name: String,
    pub content_digest: String,
}

impl SkillVersion {
    pub(crate) fn is_content_identified(&self) -> bool {
        self.name.trim() == self.name
            && !self.name.is_empty()
            && !self.name.contains('\0')
            && self.content_digest.len() == 71
            && self.content_digest.starts_with("sha256:")
            && self.content_digest[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SkillSelectionProvenance {
    Principal,
    Plan,
    Worker,
}

impl SkillSelectionProvenance {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Principal => "principal",
            Self::Plan => "plan",
            Self::Worker => "worker",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "principal" => Some(Self::Principal),
            "plan" => Some(Self::Plan),
            "worker" => Some(Self::Worker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct SelectedSkillVersion {
    pub version: SkillVersion,
    pub provenance: SkillSelectionProvenance,
}

impl SelectedSkillVersion {
    pub(crate) fn version(&self) -> SkillVersion {
        self.version.clone()
    }
}

impl std::ops::Deref for SelectedSkillVersion {
    type Target = SkillVersion;

    fn deref(&self) -> &Self::Target {
        &self.version
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentPurpose {
    CriticalPath,
    UncertaintyReduction,
    IndependentVerification,
}

impl AssignmentPurpose {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CriticalPath => "critical_path",
            Self::UncertaintyReduction => "uncertainty_reduction",
            Self::IndependentVerification => "independent_verification",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssignmentResources {
    pub concurrency_slots: u32,
    pub max_storage_bytes: u64,
    pub max_model_spend_cents: u64,
    pub max_paid_service_spend_cents: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompetitionPlan {
    pub group: String,
    pub uncertainty: String,
    pub comparison_rule: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    pub required_evidence: String,
    pub verifier_type: VerifierType,
    pub verification_depth: VerificationDepth,
    #[serde(default)]
    pub verifier_configuration: String,
    #[serde(default = "default_verification_environment")]
    pub verification_environment: String,
    pub verifier: Verifier,
}

fn default_verification_environment() -> String {
    "tyrion-controlled-v1".into()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifierType {
    #[default]
    Deterministic,
    Model,
    Principal,
}

impl VerifierType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Model => "model",
            Self::Principal => "principal",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDepth {
    #[default]
    Standard,
    Independent,
}

impl VerificationDepth {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Independent => "independent",
        }
    }

    pub(crate) const fn required_passes(self) -> usize {
        match self {
            Self::Standard => 1,
            Self::Independent => 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verifier {
    ExactMatch { expected: String },
    Command { argv: Vec<String> },
    Prompt { prompt: String },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationVerdict {
    Passed,
    Failed,
    Uncertain,
}

impl VerificationVerdict {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDefect {
    Result,
    Verifier,
    Environment,
    Criterion,
}

impl VerificationDefect {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Result => "result",
            Self::Verifier => "verifier",
            Self::Environment => "environment",
            Self::Criterion => "criterion",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidenceSubmission {
    pub criterion_id: String,
    pub result_id: String,
    pub evidence_type: String,
    pub verdict: VerificationVerdict,
    pub verifier_configuration: String,
    pub procedure: Verifier,
    pub environment: String,
    pub inspectable_output: String,
    #[serde(default)]
    pub material_contradiction: bool,
    #[serde(default)]
    pub defect: Option<VerificationDefect>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct VerificationAmendment {
    pub criteria: Vec<AcceptanceCriterion>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommissionAmendment {
    pub authority: AuthorityEnvelope,
    pub resource_ceilings: ResourceCeilings,
    pub reason: String,
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub assignment_id: String,
    pub attempt_id: String,
    pub worker_lease_id: String,
    pub mandate_revision: i64,
    pub plan_revision: i64,
    pub operation: String,
    pub repository: Option<String>,
    pub target: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    pub destination: Option<String>,
    pub effect: Option<String>,
    #[serde(default)]
    pub credential: Option<CredentialUse>,
    pub consequences: Vec<String>,
    pub limits: OperationLimits,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialUse {
    pub grant_id: String,
    pub mode: CredentialUseMode,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialUseMode {
    Brokered,
    OneShotExposure,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrantRequest {
    pub assignment_id: String,
    pub attempt_id: String,
    pub worker_lease_id: String,
    pub mandate_revision: i64,
    pub plan_revision: i64,
    pub credential_reference: String,
    pub capability: String,
    pub destination: String,
    pub exposure: CredentialExposure,
    pub credential_expires_at: i64,
    pub revocation: CredentialRevocation,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialExposure {
    BrokeredOnly,
    OneShot,
}

impl CredentialExposure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BrokeredOnly => "brokered_only",
            Self::OneShot => "one_shot",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRevocation {
    DeleteFromKeychain,
}

impl CredentialRevocation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DeleteFromKeychain => "delete_from_keychain",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationLimits {
    pub max_output_bytes: u64,
    pub max_duration_seconds: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_paid_service_spend_cents: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationReconciliationOutcome {
    Confirmed,
    NotApplied,
}

impl std::str::FromStr for OperationReconciliationOutcome {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "confirmed" => Ok(Self::Confirmed),
            "not-applied" | "not_applied" => Ok(Self::NotApplied),
            _ => Err("outcome must be confirmed or not-applied".into()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    pub protocol_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_token: Option<String>,
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
    PauseCommission {
        commission_id: String,
    },
    ResumeCommission {
        commission_id: String,
    },
    CancelCommission {
        commission_id: String,
    },
    ProposeOperation {
        commission_id: String,
        operation: Box<OperationRequest>,
    },
    GrantCredential {
        commission_id: String,
        grant: Box<CredentialGrantRequest>,
    },
    InspectApprovalGate {
        approval_gate_id: String,
    },
    ApproveOperation {
        commission_id: String,
        approval_gate_id: String,
        expected_operation_digest: String,
    },
    ExecuteOperation {
        commission_id: String,
        approval_gate_id: String,
        operation: Box<OperationRequest>,
    },
    ReconcileOperation {
        commission_id: String,
        operation_request_id: String,
        outcome: OperationReconciliationOutcome,
        observed_sha256: String,
    },
    ProposeCommissionAmendment {
        commission_id: String,
        amendment: Box<CommissionAmendment>,
    },
    InspectCommissionAmendment {
        amendment_id: String,
    },
    AcceptCommissionAmendment {
        commission_id: String,
        amendment_id: String,
        expected_amendment_digest: String,
    },
    RecordVerificationEvidence {
        commission_id: String,
        evidence: Box<VerificationEvidenceSubmission>,
    },
    AmendVerification {
        commission_id: String,
        amendment: Box<VerificationAmendment>,
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
    SteerWorker {
        commission_id: String,
        worker_handle: String,
        clarification: String,
    },
    InterruptWorker {
        commission_id: String,
        worker_handle: String,
        reason: String,
    },
    RetryWorker {
        commission_id: String,
        worker_handle: String,
    },
}

impl Command {
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::InspectCommission { .. }
                | Self::InspectApprovalGate { .. }
                | Self::InspectCommissionAmendment { .. }
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

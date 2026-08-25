use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use rustix::fs::{openat, Mode, OFlags, CWD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::credential::{
    CredentialEffectBinding, CredentialEffectError, CredentialExecutionDeadline, CredentialRuntime,
};
use crate::domain::{
    ApprovalGateStatus, AssignmentStatus, AttemptStatus, AuthorityScopeType, CommissionStatus,
    CriterionStatus, EventKind, OperationClassification, OperationStatus, ProfileClaimOutcome,
    ResultStatus, WorkerLeaseStatus,
};
use crate::protocol::{
    AcceptanceCriterion, AdapterIdentity, AssignmentPurpose, AssignmentResources,
    AttachmentHandshake, AuthorityEnvelope, CommissionAmendment, CommissionPlan,
    CommissionProposal, CommissionReplayCursor, CredentialExposure, CredentialGrantRequest,
    CredentialUseMode, ExecutionSpec, LearningObservationKind, OperationReconciliationOutcome,
    OperationRequest, PlannedAssignment, Request, ResourceCeilings, ReusablePreference,
    SelectedSkillVersion, SkillSelectionProvenance, SkillVersion, VerificationAmendment,
    VerificationDefect, VerificationDepth, VerificationEvidenceSubmission, VerificationVerdict,
    Verifier, VerifierType, WorkerRequirements, PROTOCOL_VERSION,
};
use crate::TyrionError;
use crate::{attachment, worker};

mod frontier;
mod projection;
mod schema;

use frontier::{Competition, OccupiedWork, Resources, Work};
use projection::{
    affected_attempts, event_value, inspect_commission as project_commission,
    inspect_profile as project_profile, learning_observation, learning_observations, profile_claim,
    profile_claim_lifecycle, profile_claim_versions,
};

pub struct Store {
    connection: Connection,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct EffectExecutionOptions {
    pub(crate) leave_started_before_effect: bool,
    pub(crate) leave_started_after_effect: bool,
    pub(crate) leave_one_shot_started_before_cleanup: bool,
    pub(crate) hold_before_commit_milliseconds: u64,
}

pub(crate) struct EffectExecutionContext<'a> {
    pub(crate) worker: &'a worker::WorkerRuntime,
    pub(crate) credential: Option<&'a CredentialRuntime>,
    pub(crate) options: EffectExecutionOptions,
}

pub(crate) struct EffectReconciliationContext<'a> {
    pub(crate) worker: &'a worker::WorkerRuntime,
    pub(crate) principal_token_hash: &'a str,
    pub(crate) credential: Option<&'a CredentialRuntime>,
}

pub(crate) struct ProfileClaimRevisionRequest<'a> {
    pub(crate) commission_id: &'a str,
    pub(crate) claim_id: &'a str,
    pub(crate) expected_version: i64,
    pub(crate) confirmation_digest: Option<&'a str>,
    pub(crate) preference: &'a ReusablePreference,
}

pub(crate) struct PendingCleanup {
    pub(crate) attempt_id: String,
    pub(crate) commission_id: String,
    pub(crate) execution: ExecutionSpec,
    pub(crate) artifact_revision: Option<String>,
}

struct ReadyAssignmentDispatch {
    assignment_id: String,
    logical_id: String,
    goal: String,
    execution_json: String,
    current_artifact_revision: Option<String>,
    plan_revision: i64,
    mandate_revision: i64,
    accepted_at: i64,
    max_attempts: u32,
    max_elapsed_seconds: u64,
    max_worker_concurrency: u32,
    max_storage_bytes: u64,
    max_model_spend_cents: u64,
    max_paid_service_spend_cents: u64,
    reserved_concurrency_slots: u32,
    reserved_storage_bytes: u64,
    reserved_model_spend_cents: u64,
    reserved_paid_service_spend_cents: u64,
    write_scopes: Vec<String>,
    competition_group: Option<String>,
    competition_uncertainty: Option<String>,
    competition_rule: Option<String>,
    purpose: String,
    legacy: bool,
}

struct WorkerContextSelection {
    packet: WorkerContextPacket,
    claims: Vec<ProfileClaimReference>,
    token_budget: u64,
    tokens_used: u64,
}

#[derive(Serialize)]
struct WorkerContextPacket {
    version: u8,
    precedence: [&'static str; 7],
    binding: Value,
    advisory: Value,
}

struct ProfileClaimReference {
    id: String,
    version: i64,
}

struct StoredCriterion {
    id: String,
    required_evidence: String,
    verifier_type: VerifierType,
    verification_depth: VerificationDepth,
    verifier_configuration: String,
    verification_environment: String,
    verifier_kind: String,
    expected: String,
}

struct CompletionTransition<'a> {
    commission_id: &'a str,
    result_id: &'a str,
    assignment_id: &'a str,
    attempt_id: Option<&'a str>,
    lease_id: Option<&'a str>,
    mandate_revision: i64,
    artifact_revision: &'a str,
    goal: &'a str,
}

struct PlannedAcceptance<'a> {
    assignment_id: &'a str,
    attempt_id: &'a str,
    lease_id: &'a str,
    result_id: &'a str,
    mandate_revision: i64,
}

struct SuccessfulAttemptRelease<'a> {
    attempt_id: &'a str,
    lease_id: &'a str,
}

struct VerifiedCommissionCompletion<'a> {
    commission_id: &'a str,
    mandate_revision: i64,
    artifact_revision: &'a str,
    goal: &'a str,
}

struct AttemptRecovery<'a> {
    commission_id: &'a str,
    assignment_id: &'a str,
    attempt_id: &'a str,
    cause: &'a str,
    classification: &'a str,
    equivalence_key: &'a str,
    action: &'a str,
    requirement: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FileEffectBinding {
    canonical_repository: String,
    canonical_parent: String,
    repository_device: u64,
    repository_inode: u64,
    parent_device: u64,
    parent_inode: u64,
    target_device: u64,
    target_inode: u64,
    before_sha256: String,
}

struct StoredCredentialGrant {
    id: String,
    credential_reference: String,
    capability: String,
    destination: String,
    exposure: String,
    expires_at: i64,
    status: String,
}

struct StrandedOperationRecovery {
    operation_request_id: String,
    commission_id: String,
    operation_digest: String,
    revision: i64,
    idempotency_key: Option<String>,
    request_hash: Option<String>,
    cleanup: Result<Option<Value>, String>,
}

enum BoundEffectBinding {
    File(FileEffectBinding),
    Credential(CredentialEffectBinding),
}

#[derive(Clone, Copy)]
enum AttemptContinuation {
    Current,
    Cancelled,
    Stale {
        commission_revision: i64,
        attempt_running: bool,
    },
}

#[derive(Clone, Copy)]
enum VerificationRecoveryAction {
    Rework,
    Retry,
    Reroute,
    Escalate,
    Block,
}

impl VerificationRecoveryAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Rework => "rework",
            Self::Retry => "retry",
            Self::Reroute => "reroute",
            Self::Escalate => "escalate",
            Self::Block => "block",
        }
    }
}

#[derive(Clone, Copy)]
enum VerificationRecoveryStatus {
    Pending,
    Scheduled,
    AttentionRequired,
    Blocked,
    Resolved,
}

impl VerificationRecoveryStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Scheduled => "scheduled",
            Self::AttentionRequired => "attention_required",
            Self::Blocked => "blocked",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Clone, Copy)]
enum WorkerControlAction {
    Steer,
    Interrupt,
}

impl WorkerControlAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Interrupt => "interrupt",
        }
    }

    const fn capability(self) -> &'static str {
        match self {
            Self::Steer => attachment::WORKER_STEERING,
            Self::Interrupt => attachment::WORKER_INTERRUPTION,
        }
    }

    const fn event_kind(self) -> EventKind {
        match self {
            Self::Steer => EventKind::WorkerSteered,
            Self::Interrupt => EventKind::WorkerInterrupted,
        }
    }

    const fn message_field(self) -> &'static str {
        match self {
            Self::Steer => "clarification",
            Self::Interrupt => "reason",
        }
    }
}

impl Store {
    pub fn open(database_path: &Path) -> Result<Self, TyrionError> {
        let connection = Connection::open(database_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let journal_mode =
            connection.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            connection.pragma_update(None, "journal_mode", "WAL")?;
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let initialized = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'commissions'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let migration_required = initialized && schema::migration_required(&connection)?;
        let backup_path = if migration_required {
            let backup_path = schema::migration_backup_path(database_path)?;
            schema::create_backup(&connection, &backup_path)?;
            Some(backup_path)
        } else {
            None
        };
        if !initialized || migration_required {
            connection.execute_batch(schema::SCHEMA)?;
            schema::migrate(&connection)?;
        }
        schema::verify(&connection)?;
        if let Some(backup_path) = backup_path {
            fs::remove_file(backup_path)?;
        }
        Ok(Self { connection })
    }

    pub fn create_proposal(
        &mut self,
        request: &Request,
        proposal: &CommissionProposal,
    ) -> Result<Value, TyrionError> {
        validate_proposal(proposal)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;

        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_attachment_capability(&transaction, &attachment_id, attachment::PROPOSAL_CREATION)?;
        bind_project_identity(&transaction, proposal)?;

        let commission_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO commissions (
                id, goal, status, revision, created_at, execution_json,
                worker_requirements_json, plan_json, project_id,
                commission_constraints_json
             ) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                commission_id,
                proposal.goal,
                CommissionStatus::Proposed.as_str(),
                unix_timestamp()?,
                serde_json::to_string(&proposal.execution)?,
                serde_json::to_string(&proposal.worker_requirements)?,
                proposal
                    .plan
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                proposal.project_id,
                serde_json::to_string(&proposal.commission_constraints)?,
            ],
        )?;

        for (position, criterion) in proposal.criteria.iter().enumerate() {
            let (verifier_kind, expected) = match &criterion.verifier {
                Verifier::ExactMatch { expected } => ("exact_match", expected.clone()),
                Verifier::Command { argv } => ("command", serde_json::to_string(argv)?),
                Verifier::Prompt { prompt } => ("prompt", prompt.clone()),
            };
            let verifier_configuration = resolved_verifier_configuration(criterion);
            transaction.execute(
                "INSERT INTO criteria (
                    commission_id, criterion_id, position, description, required_evidence,
                    verifier_type, verification_depth, verifier_configuration,
                    verification_environment, verifier_kind, expected, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    commission_id,
                    criterion.id,
                    position as i64,
                    criterion.description,
                    criterion.required_evidence,
                    criterion.verifier_type.as_str(),
                    criterion.verification_depth.as_str(),
                    verifier_configuration,
                    criterion.verification_environment,
                    verifier_kind,
                    expected,
                    CriterionStatus::Uncertain.as_str(),
                ],
            )?;
            insert_criterion_version(&transaction, &commission_id, 0, position, criterion)?;
        }

        insert_authority(&transaction, &commission_id, proposal)?;
        insert_resource_ceilings(&transaction, &commission_id, &proposal.resource_ceilings)?;
        for (position, uncertainty) in proposal.known_uncertainties.iter().enumerate() {
            transaction.execute(
                "INSERT INTO known_uncertainties (commission_id, position, description) VALUES (?1, ?2, ?3)",
                params![commission_id, position as i64, uncertainty],
            )?;
        }
        record_event(
            &transaction,
            &commission_id,
            EventKind::CommissionProposed,
            0,
        )?;
        transaction.execute(
            "INSERT INTO commission_attachments (commission_id, attachment_id, role, joined_at)
             VALUES (?1, ?2, 'active', ?3)",
            params![commission_id, attachment_id, unix_timestamp()?],
        )?;
        record_event_with_payload(
            &transaction,
            &commission_id,
            EventKind::AttachmentJoined,
            0,
            &serde_json::json!({
                "attachment_id": attachment_id,
                "role": "active",
            }),
        )?;
        record_event_with_payload(
            &transaction,
            &commission_id,
            EventKind::ActiveAttachmentChanged,
            0,
            &serde_json::json!({
                "previous_active_attachment_id": Value::Null,
                "active_attachment_id": attachment_id,
            }),
        )?;

        let result = project_commission(&transaction, &commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn create_profile_claim(
        &mut self,
        request: &Request,
        commission_id: &str,
        preference: &ReusablePreference,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        validate_reusable_preference(preference)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let project_id = transaction
            .query_row(
                "SELECT project_id FROM commissions WHERE id = ?1",
                [commission_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        let (scope_kind, scope_id, claim_limit, token_limit) = match project_id {
            Some(project_id) => ("project", Some(project_id), 80_u64, 4_000_u64),
            None => ("principal", None, 20_u64, 1_000_u64),
        };
        let token_upper_bound = token_upper_bound(&preference.statement);
        let statement_fingerprint = preference_fingerprint(&preference.statement);
        if let Some(boundary_id) = blocking_learning_boundary_id(
            &transaction,
            &statement_fingerprint,
            scope_kind,
            scope_id.as_deref(),
            &[],
        )? {
            return Err(TyrionError::ControlDenied(format!(
                "Learning Boundary {boundary_id} prevents Profile Claim creation"
            )));
        }
        let claim_id = Uuid::new_v4().to_string();
        let now = unix_timestamp_millis()?;
        let evicted_claim_ids = make_room_in_active_profile(
            &transaction,
            scope_kind,
            scope_id.as_deref(),
            claim_limit,
            token_limit,
            token_upper_bound,
            None,
            now,
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_versions (
                claim_id, version, statement, token_upper_bound,
                provenance_commission_id, provenance_attachment_id, created_at
             ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)",
            params![
                claim_id,
                preference.statement,
                token_upper_bound,
                commission_id,
                attachment_id,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO profile_claims (
                id, current_version, strength, scope_kind, scope_id, applicability,
                confidence_category, confidence_basis_points, lifecycle_state,
                statement_fingerprint, last_nonweak_support_at, lifecycle_changed_at,
                created_at, updated_at
             ) VALUES (?1, 1, 'hard', ?2, ?3, 'software_building',
                       'explicit', 10000, 'active', ?4, ?5, ?5, ?5, ?5)",
            params![claim_id, scope_kind, scope_id, statement_fingerprint, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (?1, NULL, 'active', 'explicit_principal_statement', ?2)",
            params![claim_id, now],
        )?;
        refresh_profile_claim_derived_data(&transaction, &claim_id, &statement_fingerprint, now)?;
        let claim = profile_claim(&transaction, &claim_id)?;
        let result = serde_json::json!({
            "claim": claim,
            "learning_receipt": {
                "kind": "profile_claim_created",
                "claim_id": claim_id,
                "claim_version": 1,
                "scope": claim["scope"],
            },
            "demoted_claim_ids": evicted_claim_ids,
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn observe_profile_preference(
        &mut self,
        request: &Request,
        commission_id: &str,
        preference: &ReusablePreference,
        outcome: LearningObservationKind,
        explanation: Option<&str>,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        validate_reusable_preference(preference)?;
        let explanation = explanation.map(str::trim).filter(|value| !value.is_empty());
        if matches!(
            outcome,
            LearningObservationKind::ExplainedRejection | LearningObservationKind::Contradiction
        ) && explanation.is_none()
        {
            return Err(TyrionError::InvalidRequest(
                "explained rejection and contradiction observations require an explanation".into(),
            ));
        }
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let (project_id, commission_status) = transaction
            .query_row(
                "SELECT project_id, status FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        let project_id = project_id.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "inferred learning requires a Commission with a verified Project".into(),
            )
        })?;
        if !matches!(
            commission_status.as_str(),
            "verified_complete" | "cancelled"
        ) {
            return Err(TyrionError::InvalidRequest(
                "learning observations require a terminal Commission".into(),
            ));
        }
        let statement_fingerprint = preference_fingerprint(&preference.statement);
        let learning_boundary_id = transaction
            .query_row(
                "SELECT id FROM learning_boundaries
                 WHERE statement_fingerprint = ?1
                   AND (
                       scope_kind = 'principal'
                       OR (scope_kind = 'project' AND scope_id = ?2)
                   )
                 ORDER BY CASE scope_kind WHEN 'project' THEN 0 ELSE 1 END, created_at, id
                 LIMIT 1",
                params![statement_fingerprint, project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(learning_boundary_id) = learning_boundary_id {
            let result = serde_json::json!({
                "observation": Value::Null,
                "claim": Value::Null,
                "promoted": false,
                "blocked_by_learning_boundary": true,
                "learning_boundary_id": learning_boundary_id,
            });
            save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
            transaction.commit()?;
            return Ok(result);
        }
        let duplicate = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM learning_observations
                WHERE commission_id = ?1 AND statement_fingerprint = ?2
             )",
            params![commission_id, statement_fingerprint],
            |row| row.get::<_, bool>(0),
        )?;
        if duplicate {
            return Err(TyrionError::InvalidRequest(
                "one Commission can contribute at most one observation per preference".into(),
            ));
        }
        let observation_id = Uuid::new_v4().to_string();
        let observed_at = unix_timestamp_millis()?;
        transaction.execute(
            "INSERT INTO learning_observations (
                id, commission_id, project_id, attachment_id, claim_id,
                statement, statement_fingerprint, kind, explanation, strength, observed_at
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                observation_id,
                commission_id,
                project_id,
                attachment_id,
                preference.statement,
                statement_fingerprint,
                outcome.as_str(),
                explanation,
                outcome.strength(),
                observed_at,
            ],
        )?;

        let mut claim_id = transaction
            .query_row(
                "SELECT id FROM profile_claims
                 WHERE scope_kind = 'project' AND scope_id = ?1
                   AND statement_fingerprint = ?2
                 ORDER BY created_at, id LIMIT 1",
                params![project_id, statement_fingerprint],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if claim_id.is_none()
            && matches!(
                outcome,
                LearningObservationKind::PrincipalEdit
                    | LearningObservationKind::ExplainedRejection
            )
        {
            let new_claim_id = Uuid::new_v4().to_string();
            let token_upper_bound = token_upper_bound(&preference.statement);
            transaction.execute(
                "INSERT INTO profile_claim_versions (
                    claim_id, version, statement, token_upper_bound,
                    provenance_commission_id, provenance_attachment_id, created_at
                 ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    new_claim_id,
                    preference.statement,
                    token_upper_bound,
                    commission_id,
                    attachment_id,
                    observed_at,
                ],
            )?;
            transaction.execute(
                "INSERT INTO profile_claims (
                    id, current_version, strength, scope_kind, scope_id, applicability,
                    confidence_category, confidence_basis_points, lifecycle_state,
                    statement_fingerprint, last_nonweak_support_at, lifecycle_changed_at,
                    created_at, updated_at
                 ) VALUES (
                    ?1, 1, 'soft', 'project', ?2, 'software_building',
                    'inferred', 5000, 'candidate', ?3, ?4, ?4, ?4, ?4
                 )",
                params![new_claim_id, project_id, statement_fingerprint, observed_at],
            )?;
            transaction.execute(
                "INSERT INTO profile_claim_lifecycle (
                    claim_id, from_state, to_state, reason, observation_id, changed_at
                 ) VALUES (?1, NULL, 'candidate', 'eligible_inferred_observation', ?2, ?3)",
                params![new_claim_id, observation_id, observed_at],
            )?;
            refresh_profile_claim_derived_data(
                &transaction,
                &new_claim_id,
                &statement_fingerprint,
                observed_at,
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
                 SELECT ?1, id FROM learning_observations
                 WHERE project_id = ?2 AND statement_fingerprint = ?3
                   AND strength IN ('strong', 'weak')",
                params![new_claim_id, project_id, statement_fingerprint],
            )?;
            transaction.execute(
                "UPDATE learning_observations SET claim_id = ?1
                 WHERE project_id = ?2 AND statement_fingerprint = ?3
                   AND claim_id IS NULL AND strength IN ('strong', 'weak')",
                params![new_claim_id, project_id, statement_fingerprint],
            )?;
            claim_id = Some(new_claim_id);
        }

        if let Some(claim_id) = claim_id.as_deref() {
            transaction.execute(
                "UPDATE learning_observations SET claim_id = ?2 WHERE id = ?1",
                params![observation_id, claim_id],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
                 VALUES (?1, ?2)",
                params![claim_id, observation_id],
            )?;
            if outcome.strength() == "strong" {
                transaction.execute(
                    "UPDATE profile_claims
                     SET last_nonweak_support_at = MAX(
                            COALESCE(last_nonweak_support_at, 0), ?2
                         ), updated_at = MAX(updated_at, ?2)
                     WHERE id = ?1",
                    params![claim_id, observed_at],
                )?;
            }
        }
        if outcome == LearningObservationKind::Contradiction {
            let mut contradicted_claims = Vec::new();
            if let Some(claim_id) = claim_id.as_deref() {
                contradicted_claims.push(claim_id.to_owned());
            }
            let principal_claim = transaction
                .query_row(
                    "SELECT id FROM profile_claims
                     WHERE scope_kind = 'principal' AND strength = 'soft'
                       AND lifecycle_state IN ('candidate', 'active')
                       AND statement_fingerprint = ?1
                     ORDER BY created_at, id LIMIT 1",
                    [statement_fingerprint.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(principal_claim) = principal_claim {
                contradicted_claims.push(principal_claim);
            }
            for contradicted_claim_id in contradicted_claims {
                let (strength, lifecycle_state) = transaction.query_row(
                    "SELECT strength, lifecycle_state FROM profile_claims WHERE id = ?1",
                    [contradicted_claim_id.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?;
                if strength == "soft" && lifecycle_state != "contradicted" {
                    transaction.execute(
                        "UPDATE profile_claims
                         SET lifecycle_state = 'contradicted', lifecycle_changed_at = ?2,
                             updated_at = ?2
                         WHERE id = ?1",
                        params![contradicted_claim_id, observed_at],
                    )?;
                    transaction.execute(
                        "INSERT INTO profile_claim_lifecycle (
                            claim_id, from_state, to_state, reason,
                            observation_id, changed_at
                         ) VALUES (?1, ?2, 'contradicted', 'current_project_evidence', ?3, ?4)",
                        params![
                            contradicted_claim_id,
                            lifecycle_state,
                            observation_id,
                            observed_at,
                        ],
                    )?;
                }
            }
        }

        let (independent_commissions, strong_observations, material_contradictions) = transaction
            .query_row(
            "SELECT COUNT(DISTINCT commission_id),
                    COALESCE(SUM(CASE WHEN strength = 'strong' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN strength = 'contradiction' THEN 1 ELSE 0 END), 0)
             FROM learning_observations
             WHERE project_id = ?1 AND statement_fingerprint = ?2
               AND strength IN ('strong', 'weak', 'contradiction')",
            params![project_id, statement_fingerprint],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )?;
        let mut promoted = false;
        if independent_commissions >= 2 && strong_observations > 0 && material_contradictions == 0 {
            if let Some(claim_id) = claim_id.as_deref() {
                let lifecycle_state = transaction.query_row(
                    "SELECT lifecycle_state FROM profile_claims WHERE id = ?1",
                    [claim_id],
                    |row| row.get::<_, String>(0),
                )?;
                if lifecycle_state == "candidate" {
                    let token_upper_bound = transaction.query_row(
                        "SELECT token_upper_bound FROM profile_claim_versions
                         JOIN profile_claims
                           ON profile_claims.id = profile_claim_versions.claim_id
                          AND profile_claims.current_version = profile_claim_versions.version
                         WHERE profile_claims.id = ?1",
                        [claim_id],
                        |row| row.get::<_, u64>(0),
                    )?;
                    if hard_profile_capacity_allows(
                        &transaction,
                        "project",
                        Some(&project_id),
                        80,
                        4_000,
                        token_upper_bound,
                    )? {
                        make_room_in_active_profile(
                            &transaction,
                            "project",
                            Some(&project_id),
                            80,
                            4_000,
                            token_upper_bound,
                            None,
                            observed_at,
                        )?;
                        transaction.execute(
                            "UPDATE profile_claims
                             SET lifecycle_state = 'active', confidence_basis_points = 7500,
                                 lifecycle_changed_at = ?2, updated_at = ?2
                             WHERE id = ?1",
                            params![claim_id, observed_at],
                        )?;
                        transaction.execute(
                            "INSERT INTO profile_claim_lifecycle (
                                claim_id, from_state, to_state, reason,
                                observation_id, changed_at
                             ) VALUES (
                                ?1, 'candidate', 'active',
                                'independent_commission_support', ?2, ?3
                             )",
                            params![claim_id, observation_id, observed_at],
                        )?;
                        promoted = true;
                    }
                }
            }
        }
        let (
            wider_commissions,
            wider_projects,
            wider_strong_observations,
            wider_material_contradictions,
        ) = transaction.query_row(
            "SELECT COUNT(DISTINCT commission_id), COUNT(DISTINCT project_id),
                        COALESCE(SUM(CASE WHEN strength = 'strong' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN strength = 'contradiction' THEN 1 ELSE 0 END), 0)
                 FROM learning_observations
                 WHERE statement_fingerprint = ?1
                   AND strength IN ('strong', 'weak', 'contradiction')",
            [statement_fingerprint.as_str()],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        )?;
        let mut principal_candidate_id = transaction
            .query_row(
                "SELECT id FROM profile_claims
                 WHERE scope_kind = 'principal' AND statement_fingerprint = ?1
                 ORDER BY created_at, id LIMIT 1",
                [statement_fingerprint.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if principal_candidate_id.is_none()
            && wider_commissions >= 3
            && wider_projects >= 2
            && wider_strong_observations > 0
            && wider_material_contradictions == 0
        {
            let new_claim_id = Uuid::new_v4().to_string();
            let token_upper_bound = token_upper_bound(&preference.statement);
            transaction.execute(
                "INSERT INTO profile_claim_versions (
                    claim_id, version, statement, token_upper_bound,
                    provenance_commission_id, provenance_attachment_id, created_at
                 ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    new_claim_id,
                    preference.statement,
                    token_upper_bound,
                    commission_id,
                    attachment_id,
                    observed_at,
                ],
            )?;
            transaction.execute(
                "INSERT INTO profile_claims (
                    id, current_version, strength, scope_kind, scope_id, applicability,
                    confidence_category, confidence_basis_points, lifecycle_state,
                    statement_fingerprint, last_nonweak_support_at, lifecycle_changed_at,
                    created_at, updated_at
                 ) VALUES (
                    ?1, 1, 'soft', 'principal', NULL, 'software_building',
                    'inferred', 7500, 'candidate', ?2, ?3, ?3, ?3, ?3
                 )",
                params![new_claim_id, statement_fingerprint, observed_at],
            )?;
            transaction.execute(
                "INSERT INTO profile_claim_lifecycle (
                    claim_id, from_state, to_state, reason, observation_id, changed_at
                 ) VALUES (
                    ?1, NULL, 'candidate', 'cross_project_support', ?2, ?3
                 )",
                params![new_claim_id, observation_id, observed_at],
            )?;
            refresh_profile_claim_derived_data(
                &transaction,
                &new_claim_id,
                &statement_fingerprint,
                observed_at,
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
                 SELECT ?1, id FROM learning_observations
                 WHERE statement_fingerprint = ?2 AND strength IN ('strong', 'weak')",
                params![new_claim_id, statement_fingerprint],
            )?;
            principal_candidate_id = Some(new_claim_id);
        } else if let Some(principal_candidate_id) = principal_candidate_id.as_deref() {
            transaction.execute(
                "INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
                 VALUES (?1, ?2)",
                params![principal_candidate_id, observation_id],
            )?;
        }
        if outcome.strength() == "strong" {
            if let Some(principal_candidate_id) = principal_candidate_id.as_deref() {
                transaction.execute(
                    "UPDATE profile_claims
                     SET last_nonweak_support_at = MAX(
                            COALESCE(last_nonweak_support_at, 0), ?2
                         ), updated_at = MAX(updated_at, ?2)
                     WHERE id = ?1",
                    params![principal_candidate_id, observed_at],
                )?;
            }
        }
        refresh_temporary_material_retention_links(&transaction, Some(commission_id))?;
        let observation = learning_observation(&transaction, &observation_id)?;
        let claim = claim_id
            .as_deref()
            .map(|claim_id| profile_claim(&transaction, claim_id))
            .transpose()?;
        let principal_candidate = principal_candidate_id
            .as_deref()
            .map(|claim_id| profile_claim(&transaction, claim_id))
            .transpose()?;
        let result = serde_json::json!({
            "observation": observation,
            "claim": claim,
            "support": {
                "independent_commissions": independent_commissions,
                "includes_principal_signal": strong_observations > 0,
                "material_contradictions": material_contradictions,
            },
            "promoted": promoted,
            "principal_candidate": principal_candidate,
            "wider_scope": {
                "independent_commissions": wider_commissions,
                "independent_projects": wider_projects,
                "material_contradictions": wider_material_contradictions,
                "requires_confirmation": principal_candidate_id.is_some(),
            },
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn confirm_profile_claim(
        &mut self,
        request: &Request,
        commission_id: &str,
        claim_id: &str,
        expected_version: i64,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let (current_version, strength, scope_kind, lifecycle_state, statement_fingerprint) =
            transaction
                .query_row(
                    "SELECT current_version, strength, scope_kind, lifecycle_state,
                        statement_fingerprint
                 FROM profile_claims WHERE id = ?1",
                    [claim_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| TyrionError::NotFound(claim_id.to_owned()))?;
        if current_version != expected_version {
            return Err(TyrionError::StaleRevision {
                expected: expected_version,
                actual: current_version,
            });
        }
        if strength != "soft" || scope_kind != "principal" || lifecycle_state != "candidate" {
            return Err(TyrionError::InvalidRequest(
                "only an inferred Principal Profile candidate can be confirmed".into(),
            ));
        }
        let related_project_ids = claim_observation_project_ids(&transaction, claim_id)?;
        if let Some(boundary_id) = blocking_learning_boundary_id(
            &transaction,
            &statement_fingerprint,
            &scope_kind,
            None,
            &related_project_ids,
        )? {
            return Err(TyrionError::ControlDenied(format!(
                "Learning Boundary {boundary_id} prevents Profile Claim reactivation"
            )));
        }
        let now = unix_timestamp_millis()?;
        let token_upper_bound = transaction.query_row(
            "SELECT token_upper_bound FROM profile_claim_versions
             WHERE claim_id = ?1 AND version = ?2",
            params![claim_id, current_version],
            |row| row.get::<_, u64>(0),
        )?;
        make_room_in_active_profile(
            &transaction,
            "principal",
            None,
            20,
            1_000,
            token_upper_bound,
            None,
            now,
        )?;
        transaction.execute(
            "UPDATE profile_claims
             SET lifecycle_state = 'active', confidence_basis_points = 9000,
                 lifecycle_changed_at = ?2, updated_at = ?2
             WHERE id = ?1",
            params![claim_id, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (
                ?1, 'candidate', 'active', 'explicit_principal_confirmation', ?2
             )",
            params![claim_id, now],
        )?;
        let claim = profile_claim(&transaction, claim_id)?;
        let result = serde_json::json!({
            "claim": claim,
            "learning_receipt": {
                "kind": "profile_claim_confirmed",
                "claim_id": claim_id,
                "claim_version": current_version,
                "scope": {"kind": "principal"},
            },
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn suppress_profile_claim(
        &mut self,
        request: &Request,
        commission_id: &str,
        claim_id: &str,
        expected_version: i64,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let source_project_id = commission_project_id(&transaction, commission_id)?;
        let (current_version, scope_kind, scope_id, lifecycle_state) = transaction
            .query_row(
                "SELECT current_version, scope_kind, scope_id, lifecycle_state
                 FROM profile_claims WHERE id = ?1",
                [claim_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(claim_id.to_owned()))?;
        ensure_claim_scope(
            &scope_kind,
            scope_id.as_deref(),
            source_project_id.as_deref(),
        )?;
        if current_version != expected_version {
            return Err(TyrionError::StaleRevision {
                expected: expected_version,
                actual: current_version,
            });
        }
        if lifecycle_state == "suppressed" {
            return Err(TyrionError::InvalidRequest(
                "the Profile Claim is already suppressed".into(),
            ));
        }
        let now = unix_timestamp_millis()?;
        transaction.execute(
            "UPDATE profile_claims
             SET lifecycle_state = 'suppressed', lifecycle_changed_at = ?2,
                 updated_at = ?2
             WHERE id = ?1",
            params![claim_id, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (?1, ?2, 'suppressed', 'principal_suppression', ?3)",
            params![claim_id, lifecycle_state, now],
        )?;
        let claim = profile_claim(&transaction, claim_id)?;
        let result = serde_json::json!({
            "claim": claim,
            "learning_receipt": {
                "kind": "profile_claim_suppressed",
                "claim_id": claim_id,
                "claim_version": current_version,
                "scope": claim["scope"],
            },
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn forget_profile_claim(
        &mut self,
        request: &Request,
        commission_id: &str,
        claim_id: &str,
        expected_version: i64,
        confirmation_digest: Option<&str>,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency = if confirmation_digest.is_some() {
            Some((mutation_key(request)?, request_hash(request)?))
        } else {
            None
        };
        let transaction = self.connection.transaction()?;
        if let Some((idempotency_key, request_hash)) = idempotency.as_ref() {
            if let Some(prior) = prior_result(&transaction, idempotency_key, request_hash)? {
                return Ok(prior);
            }
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let source_project_id = commission_project_id(&transaction, commission_id)?;
        let (current_version, scope_kind, scope_id, statement_fingerprint) = transaction
            .query_row(
                "SELECT current_version, scope_kind, scope_id, statement_fingerprint
                 FROM profile_claims WHERE id = ?1",
                [claim_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(claim_id.to_owned()))?;
        ensure_claim_scope(
            &scope_kind,
            scope_id.as_deref(),
            source_project_id.as_deref(),
        )?;
        if current_version != expected_version {
            return Err(TyrionError::StaleRevision {
                expected: expected_version,
                actual: current_version,
            });
        }
        let mut claims_to_delete = vec![claim_id.to_owned()];
        if scope_kind == "project" {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT candidate.id
                 FROM profile_claims AS candidate
                 JOIN profile_claim_observations AS candidate_observations
                   ON candidate_observations.claim_id = candidate.id
                 JOIN profile_claim_observations AS source_observations
                   ON source_observations.observation_id = candidate_observations.observation_id
                 WHERE source_observations.claim_id = ?1
                   AND candidate.id != ?1
                   AND candidate.scope_kind = 'principal'
                   AND candidate.lifecycle_state = 'candidate'
                 ORDER BY candidate.id",
            )?;
            let derived = statement
                .query_map([claim_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            claims_to_delete.extend(derived);
        }
        let observation_ids = claim_observation_ids(&transaction, &claims_to_delete)?;
        let dedicated_observation_ids =
            dedicated_observation_ids(&transaction, &observation_ids, &claims_to_delete)?;
        let claim_versions = count_for_claims(
            &transaction,
            "profile_claim_versions",
            "claim_id",
            &claims_to_delete,
        )?;
        let affected_attempts = count_for_claims(
            &transaction,
            "attempt_profile_claims",
            "claim_id",
            &claims_to_delete,
        )?
        .saturating_add(count_for_claims(
            &transaction,
            "imported_profile_claim_attempts",
            "claim_id",
            &claims_to_delete,
        )?);
        let indexes = count_for_claims(
            &transaction,
            "profile_claim_indexes",
            "claim_id",
            &claims_to_delete,
        )?;
        let caches = count_for_claims(
            &transaction,
            "profile_claim_caches",
            "claim_id",
            &claims_to_delete,
        )?;
        let remaining_related_claims = related_claim_ids(
            &transaction,
            &statement_fingerprint,
            &observation_ids,
            &claims_to_delete,
        )?;
        let cascade = serde_json::json!({
            "claims": claims_to_delete.len(),
            "claim_versions": claim_versions,
            "supporting_observations": observation_ids.len(),
            "dedicated_excerpts": dedicated_observation_ids.len(),
            "affected_attempt_records": affected_attempts,
            "indexes": indexes,
            "caches": caches,
        });
        let preview_payload = serde_json::json!({
            "operation": "forget_profile_claim",
            "commission_id": commission_id,
            "claim_id": claim_id,
            "expected_version": expected_version,
            "cascade": cascade,
            "remaining_related_claim_ids": remaining_related_claims,
        });
        let required_confirmation_digest = format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&preview_payload)?)
        );
        let Some(confirmation_digest) = confirmation_digest else {
            let result = serde_json::json!({
                "claim_id": claim_id,
                "expected_version": expected_version,
                "cascade": cascade,
                "remaining_related_claim_ids": remaining_related_claims,
                "confirmation_digest": required_confirmation_digest,
                "applied": false,
            });
            transaction.commit()?;
            return Ok(result);
        };
        if confirmation_digest != required_confirmation_digest {
            return Err(TyrionError::ControlDenied(
                "Profile Claim forgetting confirmation digest does not match the exact cascade"
                    .into(),
            ));
        }
        for claim_to_delete in &claims_to_delete {
            transaction.execute(
                "DELETE FROM memory_import_provenance
                 WHERE entity_kind = 'claim_version' AND entity_id = ?1",
                [claim_to_delete],
            )?;
            transaction.execute(
                "DELETE FROM attempt_profile_claims WHERE claim_id = ?1",
                [claim_to_delete],
            )?;
            transaction.execute(
                "DELETE FROM profile_claims WHERE id = ?1",
                [claim_to_delete],
            )?;
            transaction.execute(
                "DELETE FROM profile_claim_versions WHERE claim_id = ?1",
                [claim_to_delete],
            )?;
        }
        for observation_id in dedicated_observation_ids {
            transaction.execute(
                "DELETE FROM memory_import_provenance
                 WHERE entity_kind = 'observation' AND entity_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM profile_claim_observations WHERE observation_id = ?1
                   )",
                [&observation_id],
            )?;
            transaction.execute(
                "DELETE FROM learning_observations
                 WHERE id = ?1 AND NOT EXISTS (
                    SELECT 1 FROM profile_claim_observations WHERE observation_id = ?1
                 )",
                [observation_id],
            )?;
        }
        let deletion_receipt_id = Uuid::new_v4().to_string();
        let deleted_at = unix_timestamp_millis()?;
        transaction.execute(
            "INSERT INTO memory_deletion_receipts (
                id, claim_id, scope_kind, scope_id, cascade_json,
                remaining_related_claims_json, deleted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                deletion_receipt_id,
                claim_id,
                scope_kind,
                scope_id,
                serde_json::to_string(&cascade)?,
                serde_json::to_string(&remaining_related_claims)?,
                deleted_at,
            ],
        )?;
        let deletion_receipt = serde_json::json!({
            "id": deletion_receipt_id,
            "claim_id": claim_id,
            "scope": profile_scope(&scope_kind, scope_id.as_deref()),
            "cascade": cascade,
            "remaining_related_claim_ids": remaining_related_claims,
            "deleted_at": deleted_at,
        });
        let result = serde_json::json!({
            "applied": true,
            "deletion_receipt": deletion_receipt,
        });
        let (idempotency_key, request_hash) = idempotency
            .as_ref()
            .expect("confirmed forgetting has idempotency state");
        save_idempotent_result(&transaction, idempotency_key, request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn create_learning_boundary(
        &mut self,
        request: &Request,
        commission_id: &str,
        preference: &ReusablePreference,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        validate_reusable_preference(preference)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let project_id = commission_project_id(&transaction, commission_id)?;
        let (scope_kind, scope_id) = match project_id {
            Some(project_id) => ("project", Some(project_id)),
            None => ("principal", None),
        };
        let statement_fingerprint = preference_fingerprint(&preference.statement);
        let existing_boundary = transaction
            .query_row(
                "SELECT id FROM learning_boundaries
                 WHERE scope_kind = ?1
                   AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))
                   AND statement_fingerprint = ?3",
                params![scope_kind, scope_id, statement_fingerprint],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(existing_boundary) = existing_boundary {
            return Err(TyrionError::InvalidRequest(format!(
                "Learning Boundary {existing_boundary} already exists"
            )));
        }
        let matching_claim = transaction
            .query_row(
                "SELECT id FROM profile_claims
                 WHERE statement_fingerprint = ?1
                   AND (
                       ?2 = 'principal'
                       OR scope_kind = 'principal'
                       OR (scope_kind = 'project' AND scope_id = ?3)
                   )
                 ORDER BY CASE scope_kind WHEN 'project' THEN 0 ELSE 1 END, created_at, id
                 LIMIT 1",
                params![statement_fingerprint, scope_kind, scope_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(matching_claim) = matching_claim {
            return Err(TyrionError::ControlDenied(format!(
                "forget matching Profile Claim {matching_claim} before creating a Learning Boundary"
            )));
        }
        let boundary_id = Uuid::new_v4().to_string();
        let created_at = unix_timestamp_millis()?;
        transaction.execute(
            "INSERT INTO learning_boundaries (
                id, scope_kind, scope_id, statement_fingerprint,
                provenance_commission_id, provenance_attachment_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                boundary_id,
                scope_kind,
                scope_id,
                statement_fingerprint,
                commission_id,
                attachment_id,
                created_at,
            ],
        )?;
        let boundary = serde_json::json!({
            "id": boundary_id,
            "scope": profile_scope(scope_kind, scope_id.as_deref()),
            "statement_fingerprint": statement_fingerprint,
            "created_at": created_at,
        });
        let result = serde_json::json!({"boundary": boundary});
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn revise_profile_claim(
        &mut self,
        request: &Request,
        revision: ProfileClaimRevisionRequest<'_>,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        let ProfileClaimRevisionRequest {
            commission_id,
            claim_id,
            expected_version,
            confirmation_digest,
            preference,
        } = revision;
        authenticate_principal(request, principal_token_hash)?;
        validate_reusable_preference(preference)?;
        let idempotency = if confirmation_digest.is_some() {
            Some((mutation_key(request)?, request_hash(request)?))
        } else {
            None
        };
        let transaction = self.connection.transaction()?;
        if let Some((idempotency_key, request_hash)) = idempotency.as_ref() {
            if let Some(prior) = prior_result(&transaction, idempotency_key, request_hash)? {
                return Ok(prior);
            }
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let source_project_id = transaction
            .query_row(
                "SELECT project_id FROM commissions WHERE id = ?1",
                [commission_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        let (current_version, scope_kind, scope_id, lifecycle_state) = transaction
            .query_row(
                "SELECT current_version, scope_kind, scope_id, lifecycle_state
                 FROM profile_claims WHERE id = ?1",
                [claim_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(claim_id.to_owned()))?;
        if !matches!(lifecycle_state.as_str(), "active" | "contradicted") {
            return Err(TyrionError::InvalidRequest(
                "only an active or contradicted Profile Claim can be revised".into(),
            ));
        }
        if current_version != expected_version {
            return Err(TyrionError::StaleRevision {
                expected: expected_version,
                actual: current_version,
            });
        }
        if scope_kind == "project" && scope_id != source_project_id {
            return Err(TyrionError::ControlDenied(
                "a project Profile Claim can only be revised from the same verified Project".into(),
            ));
        }
        let current_claim = profile_claim(&transaction, claim_id)?;
        let diff = serde_json::json!({
            "statement": {
                "before": current_claim["statement"],
                "after": preference.statement,
            },
        });
        let preview_payload = serde_json::json!({
            "operation": "revise_profile_claim",
            "commission_id": commission_id,
            "claim_id": claim_id,
            "expected_version": expected_version,
            "diff": diff,
        });
        let required_confirmation_digest = format!(
            "sha256:{:x}",
            Sha256::digest(serde_json::to_vec(&preview_payload)?)
        );
        let Some(confirmation_digest) = confirmation_digest else {
            let preview = serde_json::json!({
                "claim_id": claim_id,
                "expected_version": expected_version,
                "scope": current_claim["scope"],
                "diff": diff,
                "confirmation_digest": required_confirmation_digest,
                "applied": false,
            });
            transaction.commit()?;
            return Ok(preview);
        };
        if confirmation_digest != required_confirmation_digest {
            return Err(TyrionError::ControlDenied(
                "Profile Claim revision confirmation digest does not match the exact diff".into(),
            ));
        }
        let token_upper_bound = token_upper_bound(&preference.statement);
        let (claim_limit, token_limit) = if scope_kind == "project" {
            (80, 4_000)
        } else {
            (20, 1_000)
        };
        let next_version = current_version.saturating_add(1);
        let now = unix_timestamp_millis()?;
        let statement_fingerprint = preference_fingerprint(&preference.statement);
        let related_project_ids = claim_observation_project_ids(&transaction, claim_id)?;
        if let Some(boundary_id) = blocking_learning_boundary_id(
            &transaction,
            &statement_fingerprint,
            &scope_kind,
            scope_id.as_deref(),
            &related_project_ids,
        )? {
            return Err(TyrionError::ControlDenied(format!(
                "Learning Boundary {boundary_id} prevents Profile Claim correction"
            )));
        }
        make_room_in_active_profile(
            &transaction,
            &scope_kind,
            scope_id.as_deref(),
            claim_limit,
            token_limit,
            token_upper_bound,
            Some(claim_id),
            now,
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_versions (
                claim_id, version, statement, token_upper_bound,
                provenance_commission_id, provenance_attachment_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                claim_id,
                next_version,
                preference.statement,
                token_upper_bound,
                commission_id,
                attachment_id,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE profile_claims
             SET current_version = ?2, statement_fingerprint = ?3,
                 lifecycle_state = 'active', last_nonweak_support_at = ?4,
                 lifecycle_changed_at = ?4, updated_at = ?4
             WHERE id = ?1",
            params![claim_id, next_version, statement_fingerprint, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (?1, ?2, 'active', 'principal_correction', ?3)",
            params![claim_id, lifecycle_state, now],
        )?;
        refresh_profile_claim_derived_data(&transaction, claim_id, &statement_fingerprint, now)?;
        let claim = profile_claim(&transaction, claim_id)?;
        let result = serde_json::json!({
            "claim": claim,
            "diff": diff,
            "confirmation_digest": required_confirmation_digest,
            "applied": true,
            "learning_receipt": {
                "kind": "profile_claim_changed",
                "claim_id": claim_id,
                "previous_version": current_version,
                "claim_version": next_version,
                "scope": claim["scope"],
            },
        });
        let (idempotency_key, request_hash) = idempotency
            .as_ref()
            .expect("a confirmed revision has idempotency state");
        save_idempotent_result(&transaction, idempotency_key, request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn inspect_profile_claim(
        &self,
        request: &Request,
        claim_id: &str,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        Ok(serde_json::json!({
            "claim": profile_claim(&self.connection, claim_id)?,
            "versions": profile_claim_versions(&self.connection, claim_id)?,
            "affected_attempts": affected_attempts(&self.connection, claim_id)?,
            "observations": learning_observations(&self.connection, claim_id)?,
            "lifecycle_history": profile_claim_lifecycle(&self.connection, claim_id)?,
        }))
    }

    pub fn inspect_profile(
        &self,
        request: &Request,
        project_id: Option<&str>,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        if project_id.is_some_and(|project_id| project_id.trim().is_empty()) {
            return Err(TyrionError::InvalidRequest(
                "project_id must not be empty".into(),
            ));
        }
        project_profile(&self.connection, project_id)
    }

    pub fn export_memory(
        &self,
        request: &Request,
        project_id: Option<&str>,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        if project_id.is_some_and(|project_id| project_id.trim().is_empty()) {
            return Err(TyrionError::InvalidRequest(
                "project_id must not be empty".into(),
            ));
        }
        if let Some(project_id) = project_id {
            let exists = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE project_id = ?1)",
                [project_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(TyrionError::NotFound(project_id.to_owned()));
            }
        }
        let scope_kind = if project_id.is_some() {
            "project"
        } else {
            "principal"
        };
        let claim_ids = {
            let mut statement = self.connection.prepare(
                "SELECT id FROM profile_claims
                 WHERE scope_kind = ?1
                   AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))
                 ORDER BY created_at, id",
            )?;
            let rows = statement.query_map(params![scope_kind, project_id], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut claims = Vec::new();
        for claim_id in &claim_ids {
            let (last_nonweak_support_at, lifecycle_changed_at) = self.connection.query_row(
                "SELECT last_nonweak_support_at, lifecycle_changed_at
                 FROM profile_claims WHERE id = ?1",
                [claim_id],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
            )?;
            claims.push(serde_json::json!({
                "claim": profile_claim(&self.connection, claim_id)?,
                "versions": profile_claim_versions(&self.connection, claim_id)?,
                "observations": learning_observations(&self.connection, claim_id)?,
                "lifecycle_history": profile_claim_lifecycle(&self.connection, claim_id)?,
                "affected_attempts": affected_attempts(&self.connection, claim_id)?,
                "retention": {
                    "last_nonweak_support_at": last_nonweak_support_at,
                    "lifecycle_changed_at": lifecycle_changed_at,
                },
            }));
        }
        let referenced_commission_ids = claims
            .iter()
            .flat_map(|entry| {
                entry["versions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|version| version["provenance"]["commission_id"].as_str())
                    .chain(
                        entry["observations"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|observation| observation["commission_id"].as_str()),
                    )
                    .chain(
                        entry["affected_attempts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|attempt| attempt["commission_id"].as_str()),
                    )
            })
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let profile = project_profile(&self.connection, project_id)?;
        let learning_boundaries =
            exact_scope_values(&profile["learning_boundaries"], scope_kind, project_id);
        let deletion_receipts =
            exact_scope_values(&profile["deletion_receipts"], scope_kind, project_id);
        let commission_records = export_commission_records(
            &self.connection,
            scope_kind,
            project_id,
            &referenced_commission_ids,
        )?;
        let data = serde_json::json!({
            "claims": claims,
            "learning_boundaries": learning_boundaries,
            "deletion_receipts": deletion_receipts,
            "commission_records": commission_records,
            "excluded_categories": [
                "credentials",
                "secrets",
                "session_tokens",
                "approval_artifacts",
                "raw_worker_transcripts",
                "unpinned_temporary_artifacts"
            ],
        });
        let checksum = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&data)?));
        let scope = profile_scope(scope_kind, project_id);
        let claim_lines = claims
            .iter()
            .map(|entry| {
                format!(
                    "- Profile Claim `{}` version {} ({}, {} lifecycle events, {} observations)",
                    entry["claim"]["id"].as_str().unwrap_or_default(),
                    entry["claim"]["version"].as_i64().unwrap_or_default(),
                    entry["claim"]["lifecycle"]["state"]
                        .as_str()
                        .unwrap_or_default(),
                    entry["lifecycle_history"].as_array().map_or(0, Vec::len),
                    entry["observations"].as_array().map_or(0, Vec::len),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let boundary_lines = data["learning_boundaries"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|boundary| {
                format!(
                    "- Learning Boundary `{}`",
                    boundary["id"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let deletion_lines = data["deletion_receipts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|receipt| {
                format!(
                    "- Deletion Receipt `{}` for claim `{}`",
                    receipt["id"].as_str().unwrap_or_default(),
                    receipt["claim_id"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let commission_lines = data["commission_records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|record| {
                format!(
                    "- Commission Record `{}`",
                    record["id"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary_markdown = format!(
            "# Tyrion Memory Export\n\nChecksum: `{checksum}`\n\nScope: `{}`\n\nClaims: {}\n\n{}\n\nLearning Boundaries: {}\n\n{}\n\nDeletion Receipts: {}\n\n{}\n\nCommission Records: {}\n\n{}\n",
            if let Some(project_id) = project_id {
                format!("project:{project_id}")
            } else {
                "principal".into()
            },
            claim_ids.len(),
            claim_lines,
            data["learning_boundaries"].as_array().map_or(0, Vec::len),
            boundary_lines,
            data["deletion_receipts"].as_array().map_or(0, Vec::len),
            deletion_lines,
            data["commission_records"].as_array().map_or(0, Vec::len),
            commission_lines,
        );
        Ok(serde_json::json!({
            "format": "tyrion.memory",
            "version": 1,
            "scope": scope,
            "exported_at": unix_timestamp_millis()?,
            "checksum": checksum,
            "data": data,
            "summary_markdown": summary_markdown,
        }))
    }

    pub fn import_memory(
        &mut self,
        request: &Request,
        commission_id: &str,
        bundle: &Value,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        validate_memory_bundle(bundle)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let project_id = commission_project_id(&transaction, commission_id)?;
        let bundle_scope_kind = required_json_str(&bundle["scope"], "kind")?;
        let bundle_scope_id = bundle["scope"]["project_id"].as_str();
        match (bundle_scope_kind, bundle_scope_id, project_id.as_deref()) {
            ("project", Some(bundle_project_id), Some(anchor_project_id))
                if bundle_project_id == anchor_project_id => {}
            ("principal", None, None) => {}
            _ => {
                return Err(TyrionError::ControlDenied(
                    "memory import scope must match the anchor Commission's verified scope".into(),
                ))
            }
        }
        let checksum = required_json_str(bundle, "checksum")?;
        let data = bundle["data"].as_object().ok_or_else(|| {
            TyrionError::InvalidRequest("memory export data must be an object".into())
        })?;
        let imported_at = unix_timestamp_millis()?;
        let mut commission_id_map = HashMap::new();
        let commission_records = data
            .get("commission_records")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "memory export commission_records must be an array".into(),
                )
            })?;
        for record in commission_records {
            let source_commission_id = required_json_str(record, "id")?;
            if commission_id_map.contains_key(source_commission_id) {
                return Err(TyrionError::InvalidRequest(format!(
                    "memory export repeats Commission Record {source_commission_id}"
                )));
            }
            let local_commission_id = imported_commission_id(checksum, source_commission_id);
            let source_project_id = record["project_id"].as_str().or(project_id.as_deref());
            if bundle_scope_kind == "project" && source_project_id != bundle_scope_id {
                return Err(TyrionError::InvalidRequest(
                    "project memory contains a Commission Record outside its declared scope".into(),
                ));
            }
            transaction.execute(
                "INSERT OR IGNORE INTO commissions (
                    id, goal, status, revision, control_revision, created_at,
                    completed_at, project_id
                 ) VALUES (
                    ?1, 'Imported Commission Record', 'cancelled', 1, 0, ?2, ?2, ?3
                 )",
                params![local_commission_id, imported_at, source_project_id],
            )?;
            let record_checksum =
                format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(record)?));
            let record_json = serde_json::to_string(record)?;
            let existing_record = transaction
                .query_row(
                    "SELECT record_json, checksum FROM imported_commission_records
                     WHERE record_id = ?1",
                    [source_commission_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((existing_json, existing_checksum)) = existing_record {
                if existing_json != record_json || existing_checksum != record_checksum {
                    return Err(TyrionError::InvalidRequest(format!(
                        "Commission Record {source_commission_id} conflicts with existing immutable provenance"
                    )));
                }
            } else {
                transaction.execute(
                    "INSERT INTO imported_commission_records (
                        record_id, project_id, record_json, checksum, imported_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        source_commission_id,
                        project_id,
                        record_json,
                        record_checksum,
                        imported_at,
                    ],
                )?;
            }
            commission_id_map.insert(source_commission_id.to_owned(), local_commission_id);
        }

        let mut imported_boundaries = 0_u64;
        for boundary in required_json_array(&bundle["data"], "learning_boundaries")? {
            let boundary_scope_kind = required_json_str(&boundary["scope"], "kind")?;
            let boundary_scope_id = boundary["scope"]["project_id"].as_str();
            if boundary_scope_kind != bundle_scope_kind || boundary_scope_id != bundle_scope_id {
                return Err(TyrionError::InvalidRequest(
                    "memory import contains a Learning Boundary outside its declared scope".into(),
                ));
            }
            transaction.execute(
                "INSERT INTO learning_boundaries (
                    id, scope_kind, scope_id, statement_fingerprint,
                    provenance_commission_id, provenance_attachment_id, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    required_json_str(boundary, "id")?,
                    boundary_scope_kind,
                    boundary_scope_id,
                    required_json_str(boundary, "statement_fingerprint")?,
                    commission_id,
                    attachment_id,
                    required_json_i64(boundary, "created_at")?,
                ],
            )?;
            imported_boundaries = imported_boundaries.saturating_add(1);
        }

        let claims = data
            .get("claims")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TyrionError::InvalidRequest("memory export claims must be an array".into())
            })?;
        let mut imported_claims = 0_u64;
        for entry in claims {
            let claim = entry.get("claim").ok_or_else(|| {
                TyrionError::InvalidRequest("memory claim entry is missing claim".into())
            })?;
            let claim_id = required_json_str(claim, "id")?;
            let exists = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM profile_claims WHERE id = ?1)",
                [claim_id],
                |row| row.get::<_, bool>(0),
            )?;
            if exists {
                return Err(TyrionError::InvalidRequest(format!(
                    "memory import Profile Claim {claim_id} already exists"
                )));
            }
            let claim_scope_kind = required_json_str(&claim["scope"], "kind")?;
            let claim_scope_id = claim["scope"]["project_id"].as_str();
            if claim_scope_kind != bundle_scope_kind || claim_scope_id != bundle_scope_id {
                return Err(TyrionError::InvalidRequest(
                    "memory import contains a Profile Claim outside its declared scope".into(),
                ));
            }
            let current_version = required_json_i64(claim, "version")?;
            let statement = required_json_str(claim, "statement")?;
            let statement_fingerprint = preference_fingerprint(statement);
            let related_project_ids = required_json_array(entry, "observations")?
                .iter()
                .map(|observation| required_json_str(observation, "project_id").map(str::to_owned))
                .collect::<Result<Vec<_>, _>>()?;
            if bundle_scope_kind == "project"
                && related_project_ids.iter().any(|observation_project_id| {
                    Some(observation_project_id.as_str()) != bundle_scope_id
                })
            {
                return Err(TyrionError::InvalidRequest(
                    "project memory contains an observation outside its declared scope".into(),
                ));
            }
            if let Some(boundary_id) = blocking_learning_boundary_id(
                &transaction,
                &statement_fingerprint,
                claim_scope_kind,
                claim_scope_id,
                &related_project_ids,
            )? {
                return Err(TyrionError::ControlDenied(format!(
                    "Learning Boundary {boundary_id} prevents Profile Claim import"
                )));
            }
            let versions = entry["versions"].as_array().ok_or_else(|| {
                TyrionError::InvalidRequest("memory claim versions must be an array".into())
            })?;
            if versions.is_empty() {
                return Err(TyrionError::InvalidRequest(
                    "memory Profile Claim must contain at least one version".into(),
                ));
            }
            let mut current_version_statement = None;
            for version in versions {
                let version_number = required_json_i64(version, "version")?;
                let version_statement = required_json_str(version, "statement")?;
                let declared_token_upper_bound = required_json_u64(version, "token_upper_bound")?;
                if declared_token_upper_bound != token_upper_bound(version_statement) {
                    return Err(TyrionError::InvalidRequest(
                        "memory claim token accounting does not match its statement".into(),
                    ));
                }
                if version_number == current_version {
                    current_version_statement = Some(version_statement);
                }
                let provenance = version.get("provenance").ok_or_else(|| {
                    TyrionError::InvalidRequest("memory claim version is missing provenance".into())
                })?;
                let source_commission_id = required_json_str(provenance, "commission_id")?;
                let local_commission_id = commission_id_map
                    .get(source_commission_id)
                    .map(String::as_str)
                    .ok_or_else(|| {
                        TyrionError::InvalidRequest(format!(
                            "memory claim provenance references missing Commission Record {source_commission_id}"
                        ))
                    })?;
                transaction.execute(
                    "INSERT INTO profile_claim_versions (
                        claim_id, version, statement, token_upper_bound,
                        provenance_commission_id, provenance_attachment_id, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        claim_id,
                        version_number,
                        version_statement,
                        declared_token_upper_bound,
                        local_commission_id,
                        attachment_id,
                        required_json_i64(version, "created_at")?,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO memory_import_provenance (
                        entity_kind, entity_id, entity_version, provenance_json
                     ) VALUES ('claim_version', ?1, ?2, ?3)",
                    params![
                        claim_id,
                        version_number,
                        serde_json::to_string(&serde_json::json!({
                            "kind": required_json_str(provenance, "kind")?,
                            "commission_id": source_commission_id,
                            "attachment_id": required_json_str(provenance, "attachment_id")?,
                        }))?
                    ],
                )?;
            }
            if current_version_statement != Some(statement) {
                return Err(TyrionError::InvalidRequest(
                    "memory claim head does not match its current immutable version".into(),
                ));
            }
            let retention = entry.get("retention").unwrap_or(&Value::Null);
            let last_nonweak_support_at = retention["last_nonweak_support_at"].as_i64();
            let lifecycle_changed_at = retention["lifecycle_changed_at"]
                .as_i64()
                .unwrap_or_else(|| claim["updated_at"].as_i64().unwrap_or(imported_at));
            transaction.execute(
                "INSERT INTO profile_claims (
                    id, current_version, strength, scope_kind, scope_id, applicability,
                    confidence_category, confidence_basis_points, lifecycle_state,
                    statement_fingerprint, last_nonweak_support_at, lifecycle_changed_at,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'software_building', ?6, ?7, ?8,
                           ?9, ?10, ?11, ?12, ?13)",
                params![
                    claim_id,
                    current_version,
                    required_json_str(claim, "strength")?,
                    claim_scope_kind,
                    claim_scope_id,
                    required_json_str(&claim["confidence"], "category")?,
                    required_json_u64(&claim["confidence"], "basis_points")?,
                    required_json_str(&claim["lifecycle"], "state")?,
                    statement_fingerprint,
                    last_nonweak_support_at,
                    lifecycle_changed_at,
                    required_json_i64(claim, "created_at")?,
                    required_json_i64(claim, "updated_at")?,
                ],
            )?;
            for observation in required_json_array(entry, "observations")? {
                let observation_id = required_json_str(observation, "id")?;
                let source_commission_id = required_json_str(observation, "commission_id")?;
                let local_commission_id = commission_id_map
                    .get(source_commission_id)
                    .map(String::as_str)
                    .ok_or_else(|| {
                        TyrionError::InvalidRequest(format!(
                            "memory observation references missing Commission Record {source_commission_id}"
                        ))
                    })?;
                let observation_project_id = required_json_str(observation, "project_id")?;
                let observation_statement = required_json_str(observation, "statement")?;
                let observation_fingerprint =
                    required_json_str(observation, "statement_fingerprint")?;
                let observation_kind = required_json_str(observation, "kind")?;
                let observation_explanation = observation["explanation"].as_str();
                let observation_strength = required_json_str(observation, "strength")?;
                let observation_time = required_json_i64(observation, "observed_at")?;
                let provenance = serde_json::json!({
                    "commission_id": source_commission_id,
                    "project_id": observation_project_id,
                });
                let existing_observation = transaction
                    .query_row(
                        "SELECT statement, statement_fingerprint, kind, explanation,
                                strength, observed_at,
                                (SELECT provenance_json FROM memory_import_provenance
                                 WHERE entity_kind = 'observation'
                                   AND entity_id = learning_observations.id
                                   AND entity_version = 0)
                         FROM learning_observations WHERE id = ?1",
                        [observation_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, Option<String>>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, Option<String>>(6)?,
                            ))
                        },
                    )
                    .optional()?;
                if let Some(existing) = existing_observation {
                    let existing_provenance = existing
                        .6
                        .as_deref()
                        .map(serde_json::from_str::<Value>)
                        .transpose()?;
                    if existing.0 != observation_statement
                        || existing.1 != observation_fingerprint
                        || existing.2 != observation_kind
                        || existing.3.as_deref() != observation_explanation
                        || existing.4 != observation_strength
                        || existing.5 != observation_time
                        || existing_provenance.as_ref() != Some(&provenance)
                    {
                        return Err(TyrionError::InvalidRequest(format!(
                            "memory observation identifier {observation_id} conflicts with existing immutable provenance"
                        )));
                    }
                } else {
                    transaction.execute(
                        "INSERT INTO learning_observations (
                            id, commission_id, project_id, attachment_id, claim_id,
                            statement, statement_fingerprint, kind, explanation,
                            strength, observed_at
                         ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            observation_id,
                            local_commission_id,
                            observation_project_id,
                            attachment_id,
                            observation_statement,
                            observation_fingerprint,
                            observation_kind,
                            observation_explanation,
                            observation_strength,
                            observation_time,
                        ],
                    )?;
                    transaction.execute(
                        "INSERT INTO memory_import_provenance (
                            entity_kind, entity_id, entity_version, provenance_json
                         ) VALUES ('observation', ?1, 0, ?2)",
                        params![observation_id, serde_json::to_string(&provenance)?],
                    )?;
                }
                transaction.execute(
                    "INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
                     VALUES (?1, ?2)",
                    params![claim_id, observation_id],
                )?;
            }
            for lifecycle in required_json_array(entry, "lifecycle_history")? {
                let observation_id = lifecycle["observation_id"].as_str();
                if let Some(observation_id) = observation_id {
                    let linked = transaction.query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM profile_claim_observations
                            WHERE claim_id = ?1 AND observation_id = ?2
                         )",
                        params![claim_id, observation_id],
                        |row| row.get::<_, bool>(0),
                    )?;
                    if !linked {
                        return Err(TyrionError::InvalidRequest(
                            "memory lifecycle transition references an unlinked observation".into(),
                        ));
                    }
                }
                transaction.execute(
                    "INSERT INTO profile_claim_lifecycle (
                        claim_id, from_state, to_state, reason, observation_id, changed_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        claim_id,
                        lifecycle["from_state"].as_str(),
                        required_json_str(lifecycle, "to_state")?,
                        required_json_str(lifecycle, "reason")?,
                        observation_id,
                        required_json_i64(lifecycle, "changed_at")?,
                    ],
                )?;
            }
            for (position, attempt) in required_json_array(entry, "affected_attempts")?
                .iter()
                .enumerate()
            {
                let source_commission_id = required_json_str(attempt, "commission_id")?;
                if !commission_id_map.contains_key(source_commission_id) {
                    return Err(TyrionError::InvalidRequest(format!(
                        "affected Attempt references missing Commission Record {source_commission_id}"
                    )));
                }
                transaction.execute(
                    "INSERT INTO imported_profile_claim_attempts (
                        claim_id, attempt_id, position, record_json, imported_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        claim_id,
                        required_json_str(attempt, "attempt_id")?,
                        position as i64,
                        serde_json::to_string(attempt)?,
                        imported_at,
                    ],
                )?;
            }
            refresh_profile_claim_derived_data(
                &transaction,
                claim_id,
                &preference_fingerprint(statement),
                imported_at,
            )?;
            imported_claims = imported_claims.saturating_add(1);
        }

        let mut imported_deletion_receipts = 0_u64;
        for receipt in required_json_array(&bundle["data"], "deletion_receipts")? {
            let receipt_scope_kind = required_json_str(&receipt["scope"], "kind")?;
            let receipt_scope_id = receipt["scope"]["project_id"].as_str();
            if receipt_scope_kind != bundle_scope_kind || receipt_scope_id != bundle_scope_id {
                return Err(TyrionError::InvalidRequest(
                    "memory import contains a deletion receipt outside its declared scope".into(),
                ));
            }
            transaction.execute(
                "INSERT INTO memory_deletion_receipts (
                    id, claim_id, scope_kind, scope_id, cascade_json,
                    remaining_related_claims_json, deleted_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    required_json_str(receipt, "id")?,
                    required_json_str(receipt, "claim_id")?,
                    receipt_scope_kind,
                    receipt_scope_id,
                    serde_json::to_string(&receipt["cascade"])?,
                    serde_json::to_string(&receipt["remaining_related_claim_ids"])?,
                    required_json_i64(receipt, "deleted_at")?,
                ],
            )?;
            imported_deletion_receipts = imported_deletion_receipts.saturating_add(1);
        }
        let (claim_limit, token_limit) = if bundle_scope_kind == "project" {
            (80, 4_000)
        } else {
            (20, 1_000)
        };
        let demoted_claim_ids = reconcile_active_profile_budget(
            &transaction,
            bundle_scope_kind,
            bundle_scope_id,
            claim_limit,
            token_limit,
            imported_at,
        )?;
        let result = serde_json::json!({
            "checksum": checksum,
            "scope": bundle["scope"],
            "imported": {
                "claims": imported_claims,
                "learning_boundaries": imported_boundaries,
                "deletion_receipts": imported_deletion_receipts,
                "commission_records": commission_records.len(),
                "demoted_claim_ids": demoted_claim_ids,
            },
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn pin_memory_material(
        &mut self,
        request: &Request,
        commission_id: &str,
        material_id: &str,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let updated = transaction.execute(
            "UPDATE temporary_memory_materials SET pinned = 1
             WHERE id = ?1 AND commission_id = ?2 AND content_json IS NOT NULL",
            params![material_id, commission_id],
        )?;
        if updated != 1 {
            return Err(TyrionError::NotFound(material_id.to_owned()));
        }
        let result = serde_json::json!({
            "material_id": material_id,
            "commission_id": commission_id,
            "pinned": true,
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn maintain_memory(&mut self, now_epoch_seconds: Option<i64>) -> Result<(), TyrionError> {
        let now_epoch_seconds = now_epoch_seconds.unwrap_or(unix_timestamp()?);
        if now_epoch_seconds < 0 {
            return Err(TyrionError::InvalidRequest(
                "memory maintenance time must not precede the Unix epoch".into(),
            ));
        }
        let now_millis = now_epoch_seconds.saturating_mul(1_000);
        let decay_cutoff = now_millis.saturating_sub(180_i64 * 24 * 60 * 60 * 1_000);
        let transaction = self.connection.transaction()?;
        refresh_temporary_material_retention_links(&transaction, None)?;
        let claim_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM profile_claims
                 WHERE strength = 'soft' AND lifecycle_state = 'active'
                   AND last_nonweak_support_at IS NOT NULL
                   AND last_nonweak_support_at <= ?1
                 ORDER BY last_nonweak_support_at, id",
            )?;
            let rows = statement.query_map([decay_cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for claim_id in claim_ids {
            transaction.execute(
                "UPDATE profile_claims
                 SET lifecycle_state = 'candidate', lifecycle_changed_at = ?2,
                     updated_at = ?2
                 WHERE id = ?1",
                params![claim_id, now_millis],
            )?;
            transaction.execute(
                "INSERT INTO profile_claim_lifecycle (
                    claim_id, from_state, to_state, reason, changed_at
                 ) VALUES (?1, 'active', 'candidate', 'soft_claim_decay', ?2)",
                params![claim_id, now_millis],
            )?;
        }
        let expiring_materials = {
            let mut statement = transaction.prepare(
                "SELECT id, kind, result_id FROM temporary_memory_materials
                 WHERE content_json IS NOT NULL AND expired_at IS NULL
                   AND expires_at IS NOT NULL AND expires_at <= ?1
                   AND pinned = 0 AND retained_by_evidence = 0
                   AND retained_by_claim = 0 AND retained_for_uncertain_effect = 0
                   AND commission_id IN (
                       SELECT id FROM commissions
                       WHERE status IN ('verified_complete', 'cancelled')
                   )
                 ORDER BY expires_at, id",
            )?;
            let rows = statement.query_map([now_epoch_seconds], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (material_id, kind, result_id) in expiring_materials {
            transaction.execute(
                "UPDATE temporary_memory_materials
                 SET content_json = NULL, expired_at = ?2 WHERE id = ?1",
                params![material_id, now_epoch_seconds],
            )?;
            if kind == "unaccepted_artifact" {
                if let Some(result_id) = result_id {
                    transaction.execute(
                        "UPDATE results SET artifacts_json = '[]'
                         WHERE id = ?1 AND status != 'accepted'",
                        [result_id],
                    )?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn take_control(
        &mut self,
        request: &Request,
        commission_id: &str,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest("control takeover requires an expected revision".into())
        })?;
        let expected_control_revision = request.expected_control_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "control takeover requires an expected control revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_commission_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::CONTROL_TAKEOVER,
        )?;
        let (current_revision, current_control_revision) = transaction
            .query_row(
                "SELECT revision, control_revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if current_revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if current_control_revision != expected_control_revision {
            return Err(TyrionError::StaleControlRevision {
                expected: expected_control_revision,
                actual: current_control_revision,
            });
        }
        let previous_active_attachment_id = transaction.query_row(
            "SELECT attachment_id FROM commission_attachments
             WHERE commission_id = ?1 AND role = 'active'",
            [commission_id],
            |row| row.get::<_, String>(0),
        )?;

        transaction.execute(
            "UPDATE commission_attachments SET role = 'observer'
             WHERE commission_id = ?1 AND role = 'active'",
            [commission_id],
        )?;
        transaction.execute(
            "UPDATE commission_attachments SET role = 'active'
             WHERE commission_id = ?1 AND attachment_id = ?2",
            params![commission_id, attachment_id],
        )?;
        let next_control_revision = current_control_revision + 1;
        transaction.execute(
            "UPDATE commissions SET control_revision = ?2 WHERE id = ?1",
            params![commission_id, next_control_revision],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ActiveAttachmentChanged,
            current_revision,
            &serde_json::json!({
                "previous_active_attachment_id": previous_active_attachment_id,
                "active_attachment_id": attachment_id,
                "control_revision": next_control_revision,
            }),
        )?;
        let projected = project_commission(&transaction, commission_id)?;
        let result = serde_json::json!({
            "commission_id": commission_id,
            "commission_revision": current_revision,
            "control_revision": next_control_revision,
            "active_attachment_id": attachment_id,
            "attachments": projected["attachments"],
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn steer_worker(
        &mut self,
        request: &Request,
        commission_id: &str,
        worker_handle: &str,
        clarification: &str,
        worker: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        control_worker(
            &mut self.connection,
            request,
            commission_id,
            worker_handle,
            clarification,
            WorkerControlAction::Steer,
            worker,
        )
    }

    pub fn interrupt_worker(
        &mut self,
        request: &Request,
        commission_id: &str,
        worker_handle: &str,
        reason: &str,
        worker: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        control_worker(
            &mut self.connection,
            request,
            commission_id,
            worker_handle,
            reason,
            WorkerControlAction::Interrupt,
            worker,
        )
    }

    pub fn retry_worker(
        &mut self,
        request: &Request,
        commission_id: &str,
        worker_handle: &str,
    ) -> Result<Value, TyrionError> {
        if worker_handle.trim().is_empty() {
            return Err(TyrionError::InvalidRequest(
                "Worker Handle must not be empty".into(),
            ));
        }
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Worker retry requires an expected Commission revision".into(),
            )
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::WORKER_INTERRUPTION,
        )?;
        let (status, revision) = transaction
            .query_row(
                "SELECT status, revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }
        if status != CommissionStatus::Active.as_str() {
            return Err(TyrionError::ControlDenied(format!(
                "Commission {commission_id} is {status}"
            )));
        }
        let (assignment_id, worker_status, assignment_status) = transaction
            .query_row(
                "SELECT workers.assignment_id, workers.status, assignments.status
                 FROM workers
                 JOIN assignments ON assignments.id = workers.assignment_id
                 WHERE workers.commission_id = ?1 AND workers.handle = ?2",
                params![commission_id, worker_handle],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                TyrionError::ControlDenied(format!(
                    "Worker Handle {worker_handle} is not part of Commission {commission_id}"
                ))
            })?;
        if worker_status != AttemptStatus::Interrupted.as_str()
            || assignment_status != AssignmentStatus::AttentionRequired.as_str()
        {
            return Err(TyrionError::ControlDenied(format!(
                "Worker {worker_handle} is not awaiting an explicit interruption recovery"
            )));
        }
        let retry_available = worker_retry_available(&transaction, commission_id, &assignment_id)?;
        if !retry_available {
            return Err(TyrionError::ControlDenied(
                "Worker retry requires an open interruption recovery and remaining max_attempts"
                    .into(),
            ));
        }
        transaction.execute(
            "UPDATE assignments SET status = 'ready' WHERE id = ?1",
            [&assignment_id],
        )?;
        transaction.execute(
            "UPDATE attention_conditions
             SET status = 'resolved', resolved_at = ?2
             WHERE assignment_id = ?1 AND code = 'worker_interrupted' AND status = 'open'",
            params![assignment_id, unix_timestamp()?],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::AssignmentReady,
            revision,
            &serde_json::json!({
                "assignment_id": assignment_id,
                "reason": "principal_retried_interrupted_worker",
                "prior_worker_handle": worker_handle,
                "attachment_id": attachment_id,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn pause_commission(
        &mut self,
        request: &Request,
        commission_id: &str,
    ) -> Result<Value, TyrionError> {
        self.change_commission_dispatch_state(
            request,
            commission_id,
            CommissionStatus::Active,
            CommissionStatus::Paused,
            EventKind::CommissionPaused,
        )
    }

    pub fn resume_commission(
        &mut self,
        request: &Request,
        commission_id: &str,
    ) -> Result<Value, TyrionError> {
        self.change_commission_dispatch_state(
            request,
            commission_id,
            CommissionStatus::Paused,
            CommissionStatus::Active,
            EventKind::CommissionResumed,
        )
    }

    fn change_commission_dispatch_state(
        &mut self,
        request: &Request,
        commission_id: &str,
        expected_status: CommissionStatus,
        next_status: CommissionStatus,
        event: EventKind,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(format!(
                "Commission {} requires an expected Commission revision",
                next_status.as_str()
            ))
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        let (status, revision) = transaction
            .query_row(
                "SELECT status, revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }
        if status != expected_status.as_str() {
            return Err(TyrionError::InvalidRequest(format!(
                "Commission {commission_id} is {status}; expected {}",
                expected_status.as_str()
            )));
        }
        if next_status == CommissionStatus::Active {
            let uncertain_effects = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM operation_requests
                    WHERE commission_id = ?1 AND status = 'uncertain'
                 )",
                [commission_id],
                |row| row.get::<_, bool>(0),
            )?;
            if uncertain_effects {
                return Err(TyrionError::ControlDenied(
                    "the Commission cannot resume until every uncertain consequential effect is explicitly reconciled by the Principal"
                        .into(),
                ));
            }
        }
        transaction.execute(
            "UPDATE commissions SET status = ?2 WHERE id = ?1",
            params![commission_id, next_status.as_str()],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            event,
            revision,
            &serde_json::json!({
                "attachment_id": attachment_id,
                "dispatch_enabled": next_status == CommissionStatus::Active,
                "resumable": next_status == CommissionStatus::Paused,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn cancel_commission(
        &mut self,
        request: &Request,
        commission_id: &str,
        runtime: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Commission cancellation requires an expected Commission revision".into(),
            )
        })?;
        let integration_lock = runtime.commission_integration_lock(commission_id)?;
        let _integration_guard = integration_lock.lock().map_err(|_| {
            TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        let (status, revision) = transaction
            .query_row(
                "SELECT status, revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: revision,
            });
        }
        if status != CommissionStatus::Active.as_str()
            && status != CommissionStatus::Paused.as_str()
        {
            return Err(TyrionError::InvalidRequest(format!(
                "Commission {commission_id} is {status}; only active or paused work can be cancelled"
            )));
        }
        let running_attempts = {
            let mut statement = transaction.prepare(
                "SELECT attempts.id, attempts.assignment_id
                 FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1 AND attempts.status = 'running'",
            )?;
            let rows = statement.query_map([commission_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (attempt_id, _) in &running_attempts {
            let _ = runtime.cancel_attempt(attempt_id);
        }
        let now = unix_timestamp()?;
        let now_ms = unix_timestamp_millis()?;
        let revoked_operation_request_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM operation_requests
                 WHERE commission_id = ?1 AND status IN ('approval_required', 'authorized')
                 ORDER BY proposed_at, id",
            )?;
            let rows = statement.query_map([commission_id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let affected_in_flight_operations = {
            let mut statement = transaction.prepare(
                "SELECT operation_requests.id,
                        operation_execution_identities.idempotency_key,
                        operation_execution_identities.request_hash
                 FROM operation_requests
                 LEFT JOIN operation_execution_identities
                   ON operation_execution_identities.operation_request_id = operation_requests.id
                 WHERE operation_requests.commission_id = ?1
                   AND operation_requests.status = 'started'
                 ORDER BY operation_requests.proposed_at, operation_requests.id",
            )?;
            let rows = statement.query_map([commission_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let affected_in_flight_operation_ids = affected_in_flight_operations
            .iter()
            .map(|operation| &operation.0)
            .collect::<Vec<_>>();
        let irreversible_effects = {
            let mut statement = transaction.prepare(
                "SELECT id, operation_digest, receipt_json FROM operation_requests
                 WHERE commission_id = ?1 AND status = 'confirmed'
                 ORDER BY proposed_at, id",
            )?;
            let rows = statement.query_map([commission_id], |row| {
                let receipt_json = row.get::<_, String>(2)?;
                let receipt = serde_json::from_str::<Value>(&receipt_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(serde_json::json!({
                    "operation_request_id": row.get::<_, String>(0)?,
                    "operation_digest": row.get::<_, String>(1)?,
                    "receipt": receipt,
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        transaction.execute(
            "UPDATE commissions SET status = ?2, completed_at = ?3 WHERE id = ?1",
            params![commission_id, CommissionStatus::Cancelled.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2
             WHERE commission_id = ?1 AND status NOT IN ('accepted', 'superseded')",
            params![commission_id, AssignmentStatus::Cancelled.as_str()],
        )?;
        transaction.execute(
            "UPDATE attempts
             SET status = ?2, completed_at = ?3, completed_at_ms = ?4,
                 execution_completed_at_ms = COALESCE(execution_completed_at_ms, ?4),
                 revision_disposition = 'retained'
             WHERE assignment_id IN (SELECT id FROM assignments WHERE commission_id = ?1)
               AND status = 'running'",
            params![
                commission_id,
                AttemptStatus::Cancelled.as_str(),
                now,
                now_ms
            ],
        )?;
        transaction.execute(
            "UPDATE attempt_profile_claims
             SET result_id = (
                     SELECT results.id FROM results
                     WHERE results.attempt_id = attempt_profile_claims.attempt_id
                     ORDER BY results.created_at DESC, results.rowid DESC LIMIT 1
                 ),
                 outcome = 'rejected', recorded_at = ?2
             WHERE attempt_id IN (
                 SELECT attempts.id FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1
             ) AND outcome IS NULL",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE workers
             SET status = ?2, latest_activity = 'Commission cancelled by the Principal',
                 activity_at_ms = ?3
             WHERE commission_id = ?1 AND status = 'running'",
            params![commission_id, AttemptStatus::Cancelled.as_str(), now_ms],
        )?;
        transaction.execute(
            "UPDATE worker_commands SET status = 'failed'
             WHERE commission_id = ?1 AND status = 'pending'",
            [commission_id],
        )?;
        transaction.execute(
            "UPDATE worker_leases SET status = 'revoked', released_at = ?2
             WHERE attempt_id IN (
                 SELECT attempts.id FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1
             ) AND status = 'active'",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE resource_reservations SET status = 'revoked', released_at = ?2
             WHERE commission_id = ?1 AND status = 'active'",
            params![commission_id, now_ms],
        )?;
        transaction.execute(
            "UPDATE approval_gates SET status = ?2, invalidated_at = ?3
             WHERE commission_id = ?1 AND status IN ('open', 'authorized')",
            params![commission_id, ApprovalGateStatus::Revoked.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE operation_requests SET status = ?2, completed_at = ?3
             WHERE commission_id = ?1 AND status IN ('approval_required', 'authorized')",
            params![commission_id, OperationStatus::Revoked.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE credential_grants SET status = 'revoked', revoked_at = ?2
             WHERE commission_id = ?1 AND status = 'active'",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE credential_exposure_grants SET status = 'revoked', revoked_at = ?2
             WHERE credential_grant_id IN (
                 SELECT id FROM credential_grants WHERE commission_id = ?1
             ) AND status = 'authorized'",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE operation_requests SET status = 'uncertain', completed_at = ?2,
                    receipt_json = ?3
             WHERE commission_id = ?1 AND status = 'started'",
            params![
                commission_id,
                now,
                serde_json::to_string(&serde_json::json!({
                    "status": "uncertain",
                    "requirement": "Reconcile the exact effect before any linked Commission retries it.",
                    "rollback_claimed": false,
                }))?,
            ],
        )?;
        transaction.execute(
            "UPDATE results SET revision_disposition = 'retained'
             WHERE attempt_id IN (
                 SELECT attempts.id FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1
             ) AND status != 'accepted'",
            [commission_id],
        )?;
        finalize_temporary_material_retention(&transaction, commission_id, now)?;
        for (attempt_id, assignment_id) in &running_attempts {
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: "principal_cancellation",
                    classification: "cancelled",
                    equivalence_key: "principal_cancellation",
                    action: "cancel",
                    requirement: "Create a linked Commission to continue cancelled work.",
                },
            )?;
        }
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::CommissionCancelled,
            revision,
            &serde_json::json!({
                "attachment_id": attachment_id,
                "revoked_attempt_ids": running_attempts.iter().map(|item| &item.0).collect::<Vec<_>>(),
                "authority_grants_revoked": true,
                "revoked_operation_request_ids": revoked_operation_request_ids,
                "affected_in_flight_operation_ids": affected_in_flight_operation_ids,
                "irreversible_effects": irreversible_effects,
                "rollback_claimed": false,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        for (_, execution_key, execution_hash) in &affected_in_flight_operations {
            if let (Some(execution_key), Some(execution_hash)) =
                (execution_key.as_deref(), execution_hash.as_deref())
            {
                save_idempotent_result(&transaction, execution_key, execution_hash, &result)?;
            }
        }
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn grant_credential(
        &mut self,
        request: &Request,
        commission_id: &str,
        grant: &CredentialGrantRequest,
        principal_token_hash: &str,
        runtime: Option<&CredentialRuntime>,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let runtime = runtime.ok_or_else(|| {
            TyrionError::ControlDenied("credential brokering is not configured".into())
        })?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Credential Grant issuance requires an expected Commission revision".into(),
            )
        })?;
        validate_credential_grant_shape(grant)?;
        {
            let transaction = self.connection.transaction()?;
            if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
                return Ok(prior);
            }
            transaction.commit()?;
        }
        runtime.supports_grant(
            &grant.credential_reference,
            &grant.capability,
            &grant.destination,
        )?;
        if grant.exposure == CredentialExposure::OneShot {
            runtime.supports_exposure(&grant.destination)?;
        }
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        ensure_current_credential_grant_context(
            &transaction,
            commission_id,
            grant,
            expected_revision,
        )?;
        let authority = load_authority(&transaction, commission_id)?;
        let required_action = match grant.exposure {
            CredentialExposure::BrokeredOnly => "credential.http.request",
            CredentialExposure::OneShot => "credential.command.request",
        };
        if !authority
            .destinations
            .iter()
            .any(|destination| destination == &grant.destination)
            || !authority
                .actions
                .iter()
                .any(|action| action == required_action)
            || !authority
                .effects
                .iter()
                .any(|effect| effect == "external.write")
        {
            return Err(TyrionError::ControlDenied(
                "the Credential Grant is outside the current Authority Envelope".into(),
            ));
        }
        let now = unix_timestamp()?;
        if grant.credential_expires_at <= now
            || grant.credential_expires_at > now.saturating_add(900)
        {
            return Err(TyrionError::ControlDenied(
                "a credential must be current and expire within fifteen minutes".into(),
            ));
        }
        let grant_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO credential_grants (
                id, commission_id, assignment_id, attempt_id, worker_lease_id,
                mandate_revision, plan_revision, credential_reference, capability,
                destination, exposure, credential_expires_at, revocation, status, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'active', ?14)",
            params![
                grant_id,
                commission_id,
                grant.assignment_id,
                grant.attempt_id,
                grant.worker_lease_id,
                grant.mandate_revision,
                grant.plan_revision,
                grant.credential_reference,
                grant.capability,
                grant.destination,
                grant.exposure.as_str(),
                grant.credential_expires_at,
                grant.revocation.as_str(),
                now,
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::CredentialGrantIssued,
            expected_revision,
            &serde_json::json!({
                "credential_grant_id": grant_id,
                "assignment_id": grant.assignment_id,
                "attempt_id": grant.attempt_id,
                "worker_lease_id": grant.worker_lease_id,
                "mandate_revision": grant.mandate_revision,
                "plan_revision": grant.plan_revision,
                "capability": grant.capability,
                "destination": grant.destination,
                "exposure": grant.exposure.as_str(),
                "credential_expires_at": grant.credential_expires_at,
                "credential_reference": "redacted",
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn propose_operation(
        &mut self,
        request: &Request,
        commission_id: &str,
        operation: &OperationRequest,
        credential_runtime: Option<&CredentialRuntime>,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "operation proposal requires an expected Commission revision".into(),
            )
        })?;
        validate_operation_shape(operation)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        ensure_current_operation_context(
            &transaction,
            commission_id,
            operation,
            expected_revision,
        )?;

        let authority = load_authority(&transaction, commission_id)?;
        let (initial_classification, initial_reason) = classify_operation(operation, &authority);
        let mut classification = initial_classification;
        let mut classification_reason = initial_reason.to_owned();
        let ceilings = load_resource_ceilings(&transaction, commission_id)?;
        let accepted_at = transaction.query_row(
            "SELECT accepted_at FROM commissions WHERE id = ?1",
            [commission_id],
            |row| row.get::<_, i64>(0),
        )?;
        let now = unix_timestamp()?;
        let elapsed_seconds = now.saturating_sub(accepted_at) as u64;
        let remaining_seconds = ceilings.max_elapsed_seconds.saturating_sub(elapsed_seconds);
        if operation.limits.max_duration_seconds > remaining_seconds {
            classification = OperationClassification::Prohibited;
            classification_reason = format!(
                "the exact max_duration_seconds limit is {} but only {remaining_seconds} Commission seconds remain; accept a Commission Amendment before retrying",
                operation.limits.max_duration_seconds
            );
        }
        let storage_ceiling = ceilings.max_storage_bytes;
        if operation.credential.is_some() {
            let runtime = credential_runtime.ok_or_else(|| {
                TyrionError::ControlDenied("credential brokering is not configured".into())
            })?;
            runtime.validate_operation(operation)?;
            let grant = load_current_credential_grant(&transaction, commission_id, operation)?;
            validate_credential_operation_grant(operation, &grant)?;
        }
        let projected_storage = (classification == OperationClassification::ApprovalGate
            && matches!(
                operation.operation.as_str(),
                "filesystem.write" | "credential.http.request" | "credential.command.request"
            ))
        .then(|| {
            operation
                .parameters
                .get(if operation.operation == "filesystem.write" {
                    "content"
                } else {
                    "body"
                })
                .map(|content| {
                    let content = content.len() as u64;
                    if operation.credential.is_some() {
                        content.saturating_add(operation.limits.max_output_bytes)
                    } else {
                        content
                    }
                })
        })
        .flatten();
        if let Some(projected_storage) = projected_storage {
            if operation.operation == "filesystem.write"
                && projected_storage > operation.limits.max_output_bytes
            {
                classification = OperationClassification::Prohibited;
                classification_reason = format!(
                    "filesystem.write requires {projected_storage} bytes but the exact approved max_output_bytes limit is {}",
                    operation.limits.max_output_bytes
                );
            } else if projected_storage > storage_ceiling {
                classification = OperationClassification::Prohibited;
                classification_reason = format!(
                    "the exact operation requires {projected_storage} bytes but max_storage_bytes is {storage_ceiling}; accept a Commission Amendment before retrying"
                );
            }
        }
        if operation.limits.max_paid_service_spend_cents > ceilings.max_paid_service_spend_cents {
            classification = OperationClassification::Prohibited;
            classification_reason = format!(
                "the exact operation permits {} paid-service cents but the Commission ceiling is {}",
                operation.limits.max_paid_service_spend_cents,
                ceilings.max_paid_service_spend_cents
            );
        }
        if operation.credential.is_some() {
            let (reserved_storage, reserved_paid_service) = transaction.query_row(
                "SELECT storage_bytes, paid_service_spend_cents
                 FROM resource_reservations WHERE attempt_id = ?1 AND status = 'active'",
                [&operation.attempt_id],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
            )?;
            if projected_storage.is_some_and(|required| required > reserved_storage)
                || operation.limits.max_paid_service_spend_cents > reserved_paid_service
            {
                classification = OperationClassification::Prohibited;
                classification_reason =
                    "the credentialed effect exceeds its Assignment resource grant".into();
            }
        }
        let status = match classification {
            OperationClassification::SilentJournaled
            | OperationClassification::NonBlockingNotification => OperationStatus::Completed,
            OperationClassification::ApprovalGate => OperationStatus::ApprovalRequired,
            OperationClassification::Prohibited => OperationStatus::Prohibited,
        };
        let (canonical_operation_value, _) =
            if classification == OperationClassification::ApprovalGate {
                canonical_operation(operation, credential_runtime)?
            } else {
                (serde_json::to_value(operation)?, None)
            };
        let canonical_operation = serde_json::to_string(&canonical_operation_value)?;
        let operation_digest = format!("{:x}", Sha256::digest(canonical_operation.as_bytes()));
        let operation_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO operation_requests (
                id, commission_id, assignment_id, attempt_id, worker_lease_id,
                mandate_revision, plan_revision, operation, repository, target,
                parameters_json, destination, effect, consequences_json, limits_json,
                canonical_operation_json, operation_digest, classification, status,
                classification_reason, proposed_at, completed_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                CASE WHEN ?19 = 'completed' THEN ?21 ELSE NULL END
             )",
            params![
                operation_id,
                commission_id,
                operation.assignment_id,
                operation.attempt_id,
                operation.worker_lease_id,
                operation.mandate_revision,
                operation.plan_revision,
                operation.operation,
                operation.repository,
                operation.target,
                serde_json::to_string(&operation.parameters)?,
                operation.destination,
                operation.effect,
                serde_json::to_string(&operation.consequences)?,
                serde_json::to_string(&operation.limits)?,
                canonical_operation,
                operation_digest,
                classification.as_str(),
                status.as_str(),
                classification_reason,
                now,
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::OperationClassified,
            expected_revision,
            &serde_json::json!({
                "operation_request_id": operation_id,
                "operation_digest": operation_digest,
                "classification": classification.as_str(),
                "status": status.as_str(),
                "reason": classification_reason,
            }),
        )?;
        if classification == OperationClassification::ApprovalGate {
            let approval_gate_id = Uuid::new_v4().to_string();
            transaction.execute(
                "INSERT INTO approval_gates (
                    id, commission_id, operation_request_id, operation_digest, status, opened_at
                 ) VALUES (?1, ?2, ?3, ?4, 'open', ?5)",
                params![
                    approval_gate_id,
                    commission_id,
                    operation_id,
                    operation_digest,
                    now,
                ],
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::ApprovalGateOpened,
                expected_revision,
                &serde_json::json!({
                    "approval_gate_id": approval_gate_id,
                    "operation_request_id": operation_id,
                    "operation_digest": operation_digest,
                    "canonical_operation": canonical_operation_value,
                    "exact_target": operation.target,
                    "governing_revision": {
                        "mandate": operation.mandate_revision,
                        "plan": operation.plan_revision,
                    },
                    "consequences": operation.consequences,
                    "limits": operation.limits,
                }),
            )?;
        }
        if classification == OperationClassification::NonBlockingNotification {
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::OperationNotification,
                expected_revision,
                &serde_json::json!({
                    "operation_request_id": operation_id,
                    "operation_digest": operation_digest,
                    "classification": classification.as_str(),
                    "consequences": operation.consequences,
                    "limits": operation.limits,
                }),
            )?;
        }
        if projected_storage.is_some_and(|projected| {
            classification == OperationClassification::ApprovalGate
                && projected >= storage_ceiling.saturating_sub(storage_ceiling / 5)
        }) {
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::ResourceCeilingApproaching,
                expected_revision,
                &serde_json::json!({
                    "operation_request_id": operation_id,
                    "resource": "max_storage_bytes",
                    "projected": projected_storage,
                    "ceiling": storage_ceiling,
                    "blocking": false,
                }),
            )?;
        }
        if classification != OperationClassification::Prohibited
            && operation.limits.max_duration_seconds
                >= remaining_seconds.saturating_sub(remaining_seconds / 5)
        {
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::ResourceCeilingApproaching,
                expected_revision,
                &serde_json::json!({
                    "operation_request_id": operation_id,
                    "resource": "max_elapsed_seconds",
                    "projected": elapsed_seconds.saturating_add(operation.limits.max_duration_seconds),
                    "ceiling": ceilings.max_elapsed_seconds,
                    "blocking": false,
                }),
            )?;
        }
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn inspect_approval_gate(
        &self,
        request: &Request,
        approval_gate_id: &str,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let commission_id = self
            .connection
            .query_row(
                "SELECT commission_id FROM approval_gates WHERE id = ?1",
                [approval_gate_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(approval_gate_id.to_owned()))?;
        let projection = project_commission(&self.connection, &commission_id)?;
        let gate = projection["approval_gates"]
            .as_array()
            .and_then(|gates| gates.iter().find(|gate| gate["id"] == approval_gate_id))
            .cloned()
            .ok_or_else(|| TyrionError::NotFound(approval_gate_id.to_owned()))?;
        Ok(serde_json::json!({"approval_gate": gate}))
    }

    pub fn approve_operation(
        &mut self,
        request: &Request,
        commission_id: &str,
        approval_gate_id: &str,
        expected_operation_digest: &str,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Approval Gate confirmation requires an expected Commission revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let (operation_request_id, gate_status, operation_status, stored_digest, operation_json) = transaction
            .query_row(
                "SELECT operation_requests.id, approval_gates.status, operation_requests.status,
                        approval_gates.operation_digest,
                        operation_requests.canonical_operation_json
                 FROM approval_gates
                 JOIN operation_requests
                   ON operation_requests.id = approval_gates.operation_request_id
                 WHERE approval_gates.id = ?1 AND approval_gates.commission_id = ?2",
                params![approval_gate_id, commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(approval_gate_id.to_owned()))?;
        if gate_status != ApprovalGateStatus::Open.as_str()
            || operation_status != OperationStatus::ApprovalRequired.as_str()
        {
            return Err(TyrionError::ControlDenied(
                "Approval Gate is no longer open".into(),
            ));
        }
        if stored_digest != expected_operation_digest {
            return Err(TyrionError::ControlDenied(
                "confirmation does not match the presented canonical operation".into(),
            ));
        }
        let mut operation_value: Value = serde_json::from_str(&operation_json)?;
        operation_value
            .as_object_mut()
            .ok_or_else(|| {
                TyrionError::InvalidRequest("stored canonical operation is not an object".into())
            })?
            .remove("target_revision");
        let operation: OperationRequest = serde_json::from_value(operation_value)?;
        ensure_current_operation_context(
            &transaction,
            commission_id,
            &operation,
            expected_revision,
        )?;
        if operation.credential.is_some() {
            let grant = load_current_credential_grant(&transaction, commission_id, &operation)?;
            validate_credential_operation_grant(&operation, &grant)?;
        }
        let authority = load_authority(&transaction, commission_id)?;
        if classify_operation(&operation, &authority).0 != OperationClassification::ApprovalGate {
            return Err(TyrionError::ControlDenied(
                "the operation is no longer permitted by current authority".into(),
            ));
        }
        let now = unix_timestamp()?;
        transaction.execute(
            "UPDATE approval_gates SET status = 'authorized', authorized_at = ?2
             WHERE id = ?1 AND status = 'open'",
            params![approval_gate_id, now],
        )?;
        if let Some(credential) = operation
            .credential
            .as_ref()
            .filter(|credential| credential.mode == CredentialUseMode::OneShotExposure)
        {
            let credential_grant_id = credential.grant_id.as_str();
            let exposure_grant_id = Uuid::new_v4().to_string();
            transaction.execute(
                "INSERT INTO credential_exposure_grants (
                    id, credential_grant_id, operation_request_id, operation_digest,
                    status, authorized_at
                 ) VALUES (?1, ?2, ?3, ?4, 'authorized', ?5)",
                params![
                    exposure_grant_id,
                    credential_grant_id,
                    operation_request_id,
                    stored_digest,
                    now,
                ],
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::CredentialExposureAuthorized,
                expected_revision,
                &serde_json::json!({
                    "credential_exposure_grant_id": exposure_grant_id,
                    "credential_grant_id": credential_grant_id,
                    "operation_request_id": operation_request_id,
                    "operation_digest": stored_digest,
                    "single_use": true,
                    "credential_reference": "redacted",
                }),
            )?;
        }
        transaction.execute(
            "UPDATE operation_requests SET status = 'authorized', authorized_at = ?2
             WHERE id = (SELECT operation_request_id FROM approval_gates WHERE id = ?1)
               AND status = 'approval_required'",
            params![approval_gate_id, now],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ApprovalGateAuthorized,
            expected_revision,
            &serde_json::json!({
                "approval_gate_id": approval_gate_id,
                "operation_digest": stored_digest,
                "principal_confirmation": "independent_control_credential",
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn execute_operation(
        &mut self,
        request: &Request,
        commission_id: &str,
        approval_gate_id: &str,
        operation: &OperationRequest,
        context: EffectExecutionContext<'_>,
    ) -> Result<Value, TyrionError> {
        let EffectExecutionContext {
            worker: runtime,
            credential: credential_runtime,
            options,
        } = context;
        let execution_started = Instant::now();
        let idempotency_key = mutation_key(request)?.to_owned();
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "effect execution requires an expected Commission revision".into(),
            )
        })?;
        validate_operation_shape(operation)?;
        let integration_lock = runtime.commission_integration_lock(commission_id)?;
        let _integration_guard = integration_lock.lock().map_err(|_| {
            TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, &idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        let (operation_request_id, gate_status, operation_status, stored_digest, operation_json) =
            transaction
                .query_row(
                    "SELECT operation_requests.id, approval_gates.status,
                            operation_requests.status, approval_gates.operation_digest,
                            operation_requests.canonical_operation_json
                     FROM approval_gates
                     JOIN operation_requests
                       ON operation_requests.id = approval_gates.operation_request_id
                     WHERE approval_gates.id = ?1 AND approval_gates.commission_id = ?2",
                    params![approval_gate_id, commission_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| TyrionError::NotFound(approval_gate_id.to_owned()))?;
        if gate_status != ApprovalGateStatus::Authorized.as_str()
            || operation_status != OperationStatus::Authorized.as_str()
        {
            return Err(TyrionError::ControlDenied(
                "Approval Gate is not authorized or has already been consumed".into(),
            ));
        }
        let stored_value: Value = serde_json::from_str(&operation_json)?;
        let binding_value = stored_value
            .get("target_revision")
            .cloned()
            .ok_or_else(|| {
                TyrionError::ControlDenied(
                    "the approved operation has no bound target revision".into(),
                )
            })?;
        let effect_binding = if matches!(
            operation.operation.as_str(),
            "credential.http.request" | "credential.command.request"
        ) {
            BoundEffectBinding::Credential(serde_json::from_value(binding_value.clone())?)
        } else {
            BoundEffectBinding::File(serde_json::from_value(binding_value.clone())?)
        };
        let supplied_value = canonical_operation_with_binding(operation, &binding_value)?;
        let supplied_json = serde_json::to_string(&supplied_value)?;
        let supplied_digest = format!("{:x}", Sha256::digest(supplied_json.as_bytes()));
        if supplied_digest != stored_digest || supplied_json != operation_json {
            return Err(TyrionError::ControlDenied(
                "execution does not match the exact approved canonical operation".into(),
            ));
        }
        match &effect_binding {
            BoundEffectBinding::File(binding) => ensure_file_effect_binding(operation, binding)?,
            BoundEffectBinding::Credential(binding) => credential_runtime
                .ok_or_else(|| {
                    TyrionError::ControlDenied("credential brokering is not configured".into())
                })?
                .ensure_binding(operation, binding)?,
        }
        ensure_current_operation_context(
            &transaction,
            commission_id,
            operation,
            expected_revision,
        )?;
        let authority = load_authority(&transaction, commission_id)?;
        if classify_operation(operation, &authority).0 != OperationClassification::ApprovalGate {
            return Err(TyrionError::ControlDenied(
                "the approved operation is no longer within current authority".into(),
            ));
        }
        let credential_grant = if operation.credential.is_some() {
            let grant = load_current_credential_grant(&transaction, commission_id, operation)?;
            validate_credential_operation_grant(operation, &grant)?;
            credential_runtime.ok_or_else(|| {
                TyrionError::ControlDenied("credential brokering is not configured".into())
            })?;
            Some(grant)
        } else {
            None
        };
        let now = unix_timestamp()?;
        let consumed = transaction.execute(
            "UPDATE approval_gates SET status = ?2, consumed_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                approval_gate_id,
                ApprovalGateStatus::Consumed.as_str(),
                now,
                ApprovalGateStatus::Authorized.as_str(),
            ],
        )?;
        if consumed != 1 {
            return Err(TyrionError::ControlDenied(
                "Approval Gate was already consumed".into(),
            ));
        }
        transaction.execute(
            "UPDATE operation_requests SET status = ?2, started_at = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                operation_request_id,
                OperationStatus::Started.as_str(),
                now,
                OperationStatus::Authorized.as_str(),
            ],
        )?;
        if let Some(grant) = &credential_grant {
            let consumed = transaction.execute(
                "UPDATE credential_grants SET status = 'consumed', consumed_at = ?2
                 WHERE id = ?1 AND status = 'active'",
                params![grant.id, now],
            )?;
            if consumed != 1 {
                return Err(TyrionError::ControlDenied(
                    "the Credential Grant was already consumed".into(),
                ));
            }
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::CredentialGrantConsumed,
                expected_revision,
                &serde_json::json!({
                    "credential_grant_id": grant.id,
                    "operation_request_id": operation_request_id,
                    "operation_digest": stored_digest,
                    "credential_reference": "redacted",
                }),
            )?;
            if operation
                .credential
                .as_ref()
                .is_some_and(|credential| credential.mode == CredentialUseMode::OneShotExposure)
            {
                let consumed = transaction.execute(
                    "UPDATE credential_exposure_grants
                     SET status = 'consumed', consumed_at = ?3
                     WHERE operation_request_id = ?1 AND operation_digest = ?2
                       AND credential_grant_id = ?4 AND status = 'authorized'",
                    params![operation_request_id, stored_digest, now, grant.id],
                )?;
                if consumed != 1 {
                    return Err(TyrionError::ControlDenied(
                        "the single-use Credential Exposure Grant is missing, changed, or consumed"
                            .into(),
                    ));
                }
            }
        }
        transaction.execute(
            "INSERT INTO operation_execution_identities (
                operation_request_id, idempotency_key, request_hash
             ) VALUES (?1, ?2, ?3)",
            params![operation_request_id, idempotency_key, request_hash],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::OperationStarted,
            expected_revision,
            &serde_json::json!({
                "approval_gate_id": approval_gate_id,
                "operation_request_id": operation_request_id,
                "operation_digest": stored_digest,
            }),
        )?;
        transaction.commit()?;
        if options.leave_started_before_effect {
            return Err(TyrionError::InvalidRequest(
                "fault injection left the effect durably started".into(),
            ));
        }

        let current_limits = (|| -> Result<(u64, i64), TyrionError> {
            let transaction = self.connection.transaction()?;
            ensure_current_operation_context(
                &transaction,
                commission_id,
                operation,
                expected_revision,
            )?;
            let authority = load_authority(&transaction, commission_id)?;
            if classify_operation(operation, &authority).0 != OperationClassification::ApprovalGate
            {
                return Err(TyrionError::ControlDenied(
                    "the approved operation left current authority before execution".into(),
                ));
            }
            let (
                storage,
                elapsed,
                accepted_at,
                lease_expires_at,
                assignment_storage,
                commission_paid_service,
                assignment_paid_service,
            ) = transaction.query_row(
                "SELECT resource_ceilings.max_storage_bytes,
                        resource_ceilings.max_elapsed_seconds, commissions.accepted_at,
                        worker_leases.expires_at, resource_reservations.storage_bytes,
                        resource_ceilings.max_paid_service_spend_cents,
                        resource_reservations.paid_service_spend_cents
                 FROM resource_ceilings
                 JOIN commissions ON commissions.id = resource_ceilings.commission_id
                 JOIN worker_leases ON worker_leases.id = ?2
                 JOIN resource_reservations
                   ON resource_reservations.attempt_id = worker_leases.attempt_id
                 WHERE resource_ceilings.commission_id = ?1",
                params![commission_id, operation.worker_lease_id],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, u64>(6)?,
                    ))
                },
            )?;
            let now = unix_timestamp()?;
            let elapsed_seconds = now.saturating_sub(accepted_at) as u64;
            if credential_grant
                .as_ref()
                .is_some_and(|grant| grant.expires_at <= now)
            {
                return Err(TyrionError::ControlDenied(
                    "the consumed Credential Grant expired before effect execution".into(),
                ));
            }
            if elapsed_seconds.saturating_add(operation.limits.max_duration_seconds) > elapsed {
                return Err(TyrionError::ControlDenied(
                    "the approved operation no longer fits the Commission elapsed-time ceiling"
                        .into(),
                ));
            }
            if operation.credential.is_some() {
                let required_storage = operation
                    .parameters
                    .get("body")
                    .map(|body| body.len() as u64)
                    .unwrap_or(0)
                    .saturating_add(operation.limits.max_output_bytes);
                if required_storage > storage
                    || required_storage > assignment_storage
                    || operation.limits.max_paid_service_spend_cents > commission_paid_service
                    || operation.limits.max_paid_service_spend_cents > assignment_paid_service
                {
                    return Err(TyrionError::ControlDenied(
                        "the credentialed effect no longer fits its exact Commission and Assignment resource grants"
                            .into(),
                    ));
                }
            }
            transaction.commit()?;
            Ok((
                storage,
                lease_expires_at.min(accepted_at.saturating_add(elapsed as i64)),
            ))
        })();
        let execution = match current_limits {
            Ok((commission_storage_ceiling, effect_deadline)) => match &effect_binding {
                BoundEffectBinding::File(binding) => execute_file_replacement(
                    operation,
                    commission_storage_ceiling,
                    binding,
                    effect_deadline,
                    execution_started,
                    options.leave_started_after_effect,
                    options.hold_before_commit_milliseconds,
                ),
                BoundEffectBinding::Credential(binding) => {
                    let grant = credential_grant.as_ref().ok_or_else(|| {
                        EffectExecutionError::Failed(TyrionError::ControlDenied(
                            "credentialed execution has no current Credential Grant".into(),
                        ))
                    });
                    match (credential_runtime, grant) {
                        (Some(runtime), Ok(grant)) => {
                            let deadline = CredentialExecutionDeadline::new(
                                execution_started,
                                operation.limits.max_duration_seconds,
                                effect_deadline,
                                grant.expires_at,
                            );
                            match operation
                                .credential
                                .as_ref()
                                .map(|credential| credential.mode)
                            {
                                Some(CredentialUseMode::Brokered) => runtime
                                    .execute_brokered(
                                        operation,
                                        binding,
                                        &grant.credential_reference,
                                        &operation_request_id,
                                        deadline,
                                        &mut |process_id, marker| {
                                            let transaction = self.connection.transaction_with_behavior(
                                                TransactionBehavior::Immediate,
                                            )?;
                                            let recorded = transaction.execute(
                                                "UPDATE operation_requests
                                                 SET credential_process_id = ?2,
                                                     credential_process_marker = ?3,
                                                     credential_process_status = 'active'
                                                 WHERE id = ?1 AND status = 'started'
                                                   AND credential_process_id IS NULL",
                                                params![operation_request_id, process_id, marker],
                                            )?;
                                            if recorded != 1 {
                                                return Err(TyrionError::ControlDenied(
                                                    "credential broker process identity could not be durably registered"
                                                        .into(),
                                                ));
                                            }
                                            transaction.commit()?;
                                            Ok(())
                                        },
                                    )
                                    .map_err(EffectExecutionError::from),
                                Some(CredentialUseMode::OneShotExposure) => runtime
                                    .execute_one_shot(
                                        operation,
                                        binding,
                                        &grant.credential_reference,
                                        &operation_request_id,
                                        deadline,
                                        options.leave_one_shot_started_before_cleanup,
                                    )
                                    .map_err(EffectExecutionError::from),
                                None => {
                                    Err(EffectExecutionError::Failed(TyrionError::ControlDenied(
                                        "credentialed execution has no delivery mode".into(),
                                    )))
                                }
                            }
                        }
                        (None, _) => Err(EffectExecutionError::Failed(TyrionError::ControlDenied(
                            "credential brokering is not configured".into(),
                        ))),
                        (_, Err(error)) => Err(error),
                    }
                }
            },
            Err(error) => {
                if let (Some(runtime), Some(grant)) = (credential_runtime, &credential_grant) {
                    match runtime.revoke_consumed_credential(&grant.credential_reference) {
                        Ok(()) => Err(EffectExecutionError::Failed(error)),
                        Err(revocation_error) => Err(EffectExecutionError::Uncertain {
                            error: revocation_error,
                            receipt: serde_json::json!({
                                "status": "uncertain",
                                "effect_started": false,
                                "credential_revocation": "unverified",
                                "secret_material_retained": false,
                                "requirement": "Revoke the exact credential before resuming.",
                            }),
                        }),
                    }
                } else {
                    Err(EffectExecutionError::Failed(error))
                }
            }
        };
        if matches!(
            execution,
            Err(EffectExecutionError::LeaveStartedAfterEffect)
        ) {
            return Err(TyrionError::InvalidRequest(
                "fault injection left the committed effect durably started".into(),
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if operation
            .credential
            .as_ref()
            .is_some_and(|credential| credential.mode == CredentialUseMode::Brokered)
            && credential_execution_contained(&execution)
        {
            transaction.execute(
                "UPDATE operation_requests SET credential_process_status = 'contained'
                 WHERE id = ?1 AND credential_process_status = 'active'",
                [&operation_request_id],
            )?;
        }
        match execution {
            Ok(receipt) => {
                let completed_at = unix_timestamp()?;
                transaction.execute(
                    "UPDATE operation_requests
                     SET status = 'confirmed', completed_at = ?2, receipt_json = ?3
                     WHERE id = ?1 AND status = 'started'",
                    params![
                        operation_request_id,
                        completed_at,
                        serde_json::to_string(&receipt)?,
                    ],
                )?;
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::OperationConfirmed,
                    expected_revision,
                    &serde_json::json!({
                        "approval_gate_id": approval_gate_id,
                        "operation_request_id": operation_request_id,
                        "operation_digest": stored_digest,
                        "receipt": receipt,
                    }),
                )?;
                let result = project_commission(&transaction, commission_id)?;
                save_idempotent_result(&transaction, &idempotency_key, &request_hash, &result)?;
                transaction.commit()?;
                Ok(result)
            }
            Err(EffectExecutionError::Failed(error)) => {
                let completed_at = unix_timestamp()?;
                let receipt = serde_json::json!({
                    "status": "failed",
                    "error": error.to_string(),
                    "secret_material_retained": false,
                });
                transaction.execute(
                    "UPDATE operation_requests
                     SET status = 'failed', completed_at = ?2, receipt_json = ?3
                     WHERE id = ?1 AND status = 'started'",
                    params![
                        operation_request_id,
                        completed_at,
                        serde_json::to_string(&receipt)?,
                    ],
                )?;
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::OperationFailed,
                    expected_revision,
                    &serde_json::json!({
                        "approval_gate_id": approval_gate_id,
                        "operation_request_id": operation_request_id,
                        "operation_digest": stored_digest,
                        "receipt": receipt,
                    }),
                )?;
                let result = project_commission(&transaction, commission_id)?;
                save_idempotent_result(&transaction, &idempotency_key, &request_hash, &result)?;
                transaction.commit()?;
                Ok(result)
            }
            Err(EffectExecutionError::Uncertain { error, receipt }) => {
                let completed_at = unix_timestamp()?;
                transaction.execute(
                    "UPDATE operation_requests
                     SET status = 'uncertain', completed_at = ?2, receipt_json = ?3
                     WHERE id = ?1 AND status = 'started'",
                    params![
                        operation_request_id,
                        completed_at,
                        serde_json::to_string(&receipt)?,
                    ],
                )?;
                let paused = transaction.execute(
                    "UPDATE commissions SET status = 'paused'
                     WHERE id = ?1 AND status = 'active'",
                    [commission_id],
                )?;
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::OperationUncertain,
                    expected_revision,
                    &serde_json::json!({
                        "approval_gate_id": approval_gate_id,
                        "operation_request_id": operation_request_id,
                        "operation_digest": stored_digest,
                        "receipt": receipt,
                    }),
                )?;
                if paused == 1 {
                    record_event_with_payload(
                        &transaction,
                        commission_id,
                        EventKind::CommissionPaused,
                        expected_revision,
                        &serde_json::json!({
                            "cause": "uncertain_consequential_effect",
                            "operation_request_id": operation_request_id,
                            "dispatch_enabled": false,
                            "resumable_after_principal_reconciliation": true,
                        }),
                    )?;
                }
                let result = project_commission(&transaction, commission_id)?;
                save_idempotent_result(&transaction, &idempotency_key, &request_hash, &result)?;
                transaction.commit()?;
                let _ = error;
                Ok(result)
            }
            Err(EffectExecutionError::LeaveStartedAfterEffect) => unreachable!(),
        }
    }

    pub fn reconcile_operation(
        &mut self,
        request: &Request,
        commission_id: &str,
        operation_request_id: &str,
        outcome: OperationReconciliationOutcome,
        observed_sha256: &str,
        context: EffectReconciliationContext<'_>,
    ) -> Result<Value, TyrionError> {
        let EffectReconciliationContext {
            worker,
            principal_token_hash,
            credential: credential_runtime,
        } = context;
        authenticate_principal(request, principal_token_hash)?;
        let integration_lock = worker.commission_integration_lock(commission_id)?;
        let _integration_guard = integration_lock.lock().map_err(|_| {
            TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
        })?;
        validate_sha256(observed_sha256)?;
        let idempotency_key = mutation_key(request)?.to_owned();
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "operation reconciliation requires an expected Commission revision".into(),
            )
        })?;
        let (
            canonical_json,
            binding,
            operation,
            credential_reference,
            credential_process_id,
            credential_process_marker,
            credential_process_status,
        ) = {
            let transaction = self.connection.transaction()?;
            if let Some(prior) = prior_result(&transaction, &idempotency_key, &request_hash)? {
                return Ok(prior);
            }
            let revision = transaction
                .query_row(
                    "SELECT revision FROM commissions WHERE id = ?1",
                    [commission_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
            if revision != expected_revision {
                return Err(TyrionError::StaleRevision {
                    expected: expected_revision,
                    actual: revision,
                });
            }
            let (
                status,
                canonical_json,
                credential_process_id,
                credential_process_marker,
                credential_process_status,
            ) = transaction
                .query_row(
                    "SELECT status, canonical_operation_json, credential_process_id,
                            credential_process_marker, credential_process_status
                     FROM operation_requests
                     WHERE id = ?1 AND commission_id = ?2",
                    params![operation_request_id, commission_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<u32>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| TyrionError::NotFound(operation_request_id.to_owned()))?;
            if status != OperationStatus::Uncertain.as_str() {
                return Err(TyrionError::ControlDenied(
                    "only an uncertain consequential effect can be reconciled".into(),
                ));
            }
            let mut canonical: Value = serde_json::from_str(&canonical_json)?;
            let binding_value = canonical.get("target_revision").cloned().ok_or_else(|| {
                TyrionError::InvalidRequest("uncertain effect has no exact target revision".into())
            })?;
            canonical
                .as_object_mut()
                .ok_or_else(|| {
                    TyrionError::InvalidRequest("canonical operation must be an object".into())
                })?
                .remove("target_revision");
            let operation: OperationRequest = serde_json::from_value(canonical)?;
            let binding = if operation.credential.is_some() {
                BoundEffectBinding::Credential(serde_json::from_value(binding_value)?)
            } else {
                BoundEffectBinding::File(serde_json::from_value(binding_value)?)
            };
            let credential_reference = if let Some(credential) = &operation.credential {
                Some(
                    transaction
                        .query_row(
                            "SELECT credential_reference FROM credential_grants WHERE id = ?1",
                            [&credential.grant_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                        .ok_or_else(|| {
                            TyrionError::ControlDenied(
                                "uncertain effect has no durable Credential Grant".into(),
                            )
                        })?,
                )
            } else {
                None
            };
            transaction.commit()?;
            (
                canonical_json,
                binding,
                operation,
                credential_reference,
                credential_process_id,
                credential_process_marker,
                credential_process_status,
            )
        };
        if let Some(credential_reference) = credential_reference.as_deref() {
            let runtime = credential_runtime.ok_or_else(|| {
                TyrionError::ControlDenied(
                    "credentialed reconciliation requires its pinned cleanup runtime".into(),
                )
            })?;
            runtime.recover_stranded_effect(
                operation_request_id,
                &operation,
                credential_reference,
                active_broker_process(
                    credential_process_id,
                    credential_process_marker.as_deref(),
                    credential_process_status.as_deref(),
                )?,
            )?;
        }
        let (actual_sha256, expected_sha256, observer) = match &binding {
            BoundEffectBinding::File(binding) => {
                let actual = observe_effect_target(&operation, binding)?;
                let expected = match outcome {
                    OperationReconciliationOutcome::Confirmed => {
                        let content = operation.parameters.get("content").ok_or_else(|| {
                            TyrionError::InvalidRequest(
                                "filesystem.write reconciliation requires content".into(),
                            )
                        })?;
                        format!("{:x}", Sha256::digest(content.as_bytes()))
                    }
                    OperationReconciliationOutcome::NotApplied => binding.before_sha256.clone(),
                };
                (actual, expected, "daemon_filesystem_observer")
            }
            BoundEffectBinding::Credential(binding) => {
                let runtime = credential_runtime.ok_or_else(|| {
                    TyrionError::ControlDenied(
                        "credentialed reconciliation requires its pinned broker runtime".into(),
                    )
                })?;
                let actual = runtime.observe_read_only(&operation, binding)?;
                let field = match outcome {
                    OperationReconciliationOutcome::Confirmed => "confirmed_reconciliation_sha256",
                    OperationReconciliationOutcome::NotApplied => {
                        "not_applied_reconciliation_sha256"
                    }
                };
                let expected = operation.parameters.get(field).cloned().ok_or_else(|| {
                    TyrionError::InvalidRequest(format!(
                        "credentialed reconciliation requires {field}"
                    ))
                })?;
                validate_sha256(&expected)?;
                (actual, expected, "brokered_read_only_observer")
            }
        };
        if observed_sha256 != actual_sha256 || observed_sha256 != expected_sha256 {
            return Err(TyrionError::ControlDenied(
                "Principal reconciliation does not match the independently observed exact target revision"
                    .into(),
            ));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior) = prior_result(&transaction, &idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let (revision, status, current_canonical_json) = transaction.query_row(
            "SELECT commissions.revision, operation_requests.status,
                        operation_requests.canonical_operation_json
                 FROM operation_requests
                 JOIN commissions ON commissions.id = operation_requests.commission_id
                 WHERE operation_requests.id = ?1 AND operation_requests.commission_id = ?2",
            params![operation_request_id, commission_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        if revision != expected_revision
            || status != OperationStatus::Uncertain.as_str()
            || current_canonical_json != canonical_json
        {
            return Err(TyrionError::ControlDenied(
                "the uncertain effect changed while Principal reconciliation was in progress"
                    .into(),
            ));
        }
        if credential_reference.is_some() {
            transaction.execute(
                "UPDATE operation_requests SET credential_process_status = 'contained'
                 WHERE id = ?1 AND credential_process_status = 'active'",
                [operation_request_id],
            )?;
        }
        let (status, event, reconciliation) = match outcome {
            OperationReconciliationOutcome::Confirmed => (
                OperationStatus::Confirmed,
                EventKind::OperationConfirmed,
                "confirmed_after_restart",
            ),
            OperationReconciliationOutcome::NotApplied => (
                OperationStatus::Failed,
                EventKind::OperationFailed,
                "confirmed_not_applied",
            ),
        };
        let reconciled_at = unix_timestamp()?;
        let receipt = serde_json::json!({
            "status": status.as_str(),
            "reconciliation": reconciliation,
            "principal_observed_sha256": observed_sha256,
            "independently_observed_sha256": actual_sha256,
            "reconciliation_observer": observer,
            "rollback_claimed": false,
            "secret_material_retained": false,
        });
        transaction.execute(
            "UPDATE operation_requests
             SET status = ?2, completed_at = ?3, receipt_json = ?4
             WHERE id = ?1 AND status = 'uncertain'",
            params![
                operation_request_id,
                status.as_str(),
                reconciled_at,
                serde_json::to_string(&receipt)?,
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            event,
            revision,
            &serde_json::json!({
                "operation_request_id": operation_request_id,
                "principal_reconciliation": reconciliation,
                "observed_sha256": observed_sha256,
                "receipt": receipt,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, &idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn propose_commission_amendment(
        &mut self,
        request: &Request,
        commission_id: &str,
        amendment: &CommissionAmendment,
    ) -> Result<Value, TyrionError> {
        validate_commission_amendment(amendment)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Commission Amendment proposal requires an expected revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        let (status, current_revision) = transaction
            .query_row(
                "SELECT status, revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if current_revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if status != CommissionStatus::Active.as_str()
            && status != CommissionStatus::Paused.as_str()
        {
            return Err(TyrionError::InvalidRequest(format!(
                "Commission {commission_id} is {status}; only an active or paused mandate can be amended"
            )));
        }
        let has_pending_amendment = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM commission_amendments
                WHERE commission_id = ?1 AND status = 'proposed'
             )",
            [commission_id],
            |row| row.get::<_, bool>(0),
        )?;
        if has_pending_amendment {
            return Err(TyrionError::InvalidRequest(
                "the Commission already has a proposed Amendment".into(),
            ));
        }
        let current_authority = load_authority(&transaction, commission_id)?;
        let current_ceilings = load_resource_ceilings(&transaction, commission_id)?;
        let diff = commission_amendment_diff(
            &current_authority,
            &amendment.authority,
            &current_ceilings,
            &amendment.resource_ceilings,
        );
        if diff["changed"] != true {
            return Err(TyrionError::InvalidRequest(
                "Commission Amendment must contain an exact authority or resource change".into(),
            ));
        }
        validate_amended_execution_authority(&transaction, commission_id, &amendment.authority)?;
        validate_amended_resource_ceilings(
            &transaction,
            commission_id,
            &amendment.resource_ceilings,
        )?;
        let canonical_amendment = serde_json::json!({
            "base_revision": current_revision,
            "amendment": amendment,
        });
        let amendment_digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&canonical_amendment)?)
        );
        let active_leases = query_active_lease_impact(&transaction, commission_id)?;
        let affected_operations = query_open_operation_ids(&transaction, commission_id)?;
        let impact = serde_json::json!({
            "worker_leases": active_leases,
            "operation_request_ids_requiring_revalidation": affected_operations,
            "revalidation_required_before_effect_execution": true,
        });
        let amendment_id = Uuid::new_v4().to_string();
        let proposed_at = unix_timestamp()?;
        transaction.execute(
            "INSERT INTO commission_amendments (
                id, commission_id, base_revision, authority_json, resource_ceilings_json,
                reason, diff_json, amendment_digest, impact_json, status, proposed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'proposed', ?10)",
            params![
                amendment_id,
                commission_id,
                current_revision,
                serde_json::to_string(&amendment.authority)?,
                serde_json::to_string(&amendment.resource_ceilings)?,
                amendment.reason,
                serde_json::to_string(&diff)?,
                amendment_digest,
                serde_json::to_string(&impact)?,
                proposed_at,
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::CommissionAmendmentProposed,
            current_revision,
            &serde_json::json!({
                "amendment_id": amendment_id,
                "amendment_digest": amendment_digest,
                "diff": diff,
                "impact": impact,
                "attachment_id": attachment_id,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn inspect_commission_amendment(
        &self,
        request: &Request,
        amendment_id: &str,
        principal_token_hash: &str,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let commission_id = self
            .connection
            .query_row(
                "SELECT commission_id FROM commission_amendments WHERE id = ?1",
                [amendment_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(amendment_id.to_owned()))?;
        let projection = project_commission(&self.connection, &commission_id)?;
        let amendment = projection["commission_amendments"]
            .as_array()
            .and_then(|amendments| {
                amendments
                    .iter()
                    .find(|amendment| amendment["id"] == amendment_id)
            })
            .cloned()
            .ok_or_else(|| TyrionError::NotFound(amendment_id.to_owned()))?;
        Ok(serde_json::json!({"commission_amendment": amendment}))
    }

    pub fn accept_commission_amendment(
        &mut self,
        request: &Request,
        commission_id: &str,
        amendment_id: &str,
        expected_amendment_digest: &str,
        principal_token_hash: &str,
        runtime: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        authenticate_principal(request, principal_token_hash)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Commission Amendment acceptance requires an expected revision".into(),
            )
        })?;
        let integration_lock = runtime.commission_integration_lock(commission_id)?;
        let _integration_guard = integration_lock.lock().map_err(|_| {
            TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let (base_revision, authority_json, ceilings_json, stored_digest, amendment_status) =
            transaction
                .query_row(
                    "SELECT base_revision, authority_json, resource_ceilings_json,
                            amendment_digest, status
                     FROM commission_amendments WHERE id = ?1 AND commission_id = ?2",
                    params![amendment_id, commission_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| TyrionError::NotFound(amendment_id.to_owned()))?;
        if amendment_status != "proposed" {
            return Err(TyrionError::ControlDenied(
                "Commission Amendment is no longer proposed".into(),
            ));
        }
        if stored_digest != expected_amendment_digest {
            return Err(TyrionError::ControlDenied(
                "acceptance does not match the exact proposed Commission Amendment".into(),
            ));
        }
        let (commission_status, current_revision) = transaction.query_row(
            "SELECT status, revision FROM commissions WHERE id = ?1",
            [commission_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if current_revision != expected_revision || base_revision != current_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if commission_status != CommissionStatus::Active.as_str()
            && commission_status != CommissionStatus::Paused.as_str()
        {
            return Err(TyrionError::ControlDenied(
                "only an active or paused Commission can accept an Amendment".into(),
            ));
        }
        let authority: AuthorityEnvelope = serde_json::from_str(&authority_json)?;
        let ceilings: ResourceCeilings = serde_json::from_str(&ceilings_json)?;
        validate_amended_execution_authority(&transaction, commission_id, &authority)?;
        validate_amended_resource_ceilings(&transaction, commission_id, &ceilings)?;

        replace_authority(&transaction, commission_id, &authority)?;
        replace_resource_ceilings(&transaction, commission_id, &ceilings)?;
        let mandate_revision = current_revision + 1;
        transaction.execute(
            "UPDATE commissions SET revision = ?2 WHERE id = ?1",
            params![commission_id, mandate_revision],
        )?;
        copy_current_criterion_versions(&transaction, commission_id, mandate_revision)?;

        let execution_json = transaction.query_row(
            "SELECT execution_json FROM commissions WHERE id = ?1",
            [commission_id],
            |row| row.get::<_, String>(0),
        )?;
        let execution: ExecutionSpec = serde_json::from_str(&execution_json)?;
        let active_leases = query_active_leases(&transaction, commission_id)?;
        let mut lease_revalidation = Vec::new();
        for (lease_id, attempt_id) in active_leases {
            let authority_current =
                attempt_authority_is_current(&transaction, commission_id, &attempt_id, &execution)?;
            let resources_current =
                attempt_resources_fit(&transaction, commission_id, &attempt_id, &ceilings)?;
            let _ = runtime.cancel_attempt(&attempt_id);
            let now = unix_timestamp()?;
            let now_ms = unix_timestamp_millis()?;
            transaction.execute(
                "UPDATE worker_leases SET status = 'revoked', released_at = ?2
                 WHERE id = ?1 AND status = 'active'",
                params![lease_id, now],
            )?;
            transaction.execute(
                "UPDATE resource_reservations SET status = 'revoked', released_at = ?2
                 WHERE attempt_id = ?1 AND status = 'active'",
                params![attempt_id, now_ms],
            )?;
            transaction.execute(
                "UPDATE attempts
                 SET status = 'cancelled', completed_at = ?2, completed_at_ms = ?3,
                     execution_completed_at_ms = COALESCE(execution_completed_at_ms, ?3),
                     revision_disposition = 'requires_revalidation'
                 WHERE id = ?1 AND status = 'running'",
                params![attempt_id, now, now_ms],
            )?;
            transaction.execute(
                "UPDATE workers
                 SET status = 'cancelled', latest_activity = ?2, activity_at_ms = ?3
                 WHERE attempt_id = ?1 AND status = 'running'",
                params![
                    attempt_id,
                    "Commission Amendment requires a revision-bound replacement Attempt",
                    now_ms,
                ],
            )?;
            record_attempt_profile_claim_outcome(
                &transaction,
                &attempt_id,
                ProfileClaimOutcome::Rejected,
            )?;
            transaction.execute(
                "UPDATE assignments SET status = 'ready'
                 WHERE id = (SELECT assignment_id FROM attempts WHERE id = ?1)
                   AND status = 'running'",
                [&attempt_id],
            )?;
            lease_revalidation.push(serde_json::json!({
                "worker_lease_id": lease_id,
                "attempt_id": attempt_id,
                "outcome": "restart_required",
                "authority_current": authority_current,
                "resources_current": resources_current,
                "replacement_mandate_revision": mandate_revision,
            }));
        }
        let invalidated_operation_ids = query_open_operation_ids(&transaction, commission_id)?;
        let now = unix_timestamp()?;
        transaction.execute(
            "UPDATE approval_gates
             SET status = ?2, invalidated_at = ?3
             WHERE commission_id = ?1 AND status IN ('open', 'authorized')",
            params![commission_id, ApprovalGateStatus::Invalidated.as_str(), now,],
        )?;
        transaction.execute(
            "UPDATE operation_requests SET status = 'revoked', completed_at = ?2
             WHERE commission_id = ?1 AND status IN ('approval_required', 'authorized')",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE credential_grants SET status = 'revoked', revoked_at = ?2
             WHERE commission_id = ?1 AND status = 'active'",
            params![commission_id, now],
        )?;
        transaction.execute(
            "UPDATE credential_exposure_grants SET status = 'revoked', revoked_at = ?2
             WHERE credential_grant_id IN (
                 SELECT id FROM credential_grants WHERE commission_id = ?1
             ) AND status = 'authorized'",
            params![commission_id, now],
        )?;
        let revalidation = serde_json::json!({
            "worker_leases": lease_revalidation,
            "invalidated_operation_request_ids": invalidated_operation_ids,
            "criteria_rebound_to_mandate_revision": mandate_revision,
        });
        transaction.execute(
            "UPDATE commission_amendments
             SET status = 'accepted', accepted_at = ?2, revalidation_json = ?3
             WHERE id = ?1 AND status = 'proposed'",
            params![amendment_id, now, serde_json::to_string(&revalidation)?],
        )?;
        transaction.execute(
            "UPDATE commission_amendments SET status = 'invalidated'
             WHERE commission_id = ?1 AND id != ?2 AND status = 'proposed'",
            params![commission_id, amendment_id],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::CommissionAmended,
            mandate_revision,
            &serde_json::json!({
                "amendment_id": amendment_id,
                "amendment_digest": stored_digest,
                "base_revision": base_revision,
                "mandate_revision": mandate_revision,
                "revalidation": revalidation,
            }),
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn issue_attachment_token(
        &mut self,
        request: &Request,
        expected_adapter: &AdapterIdentity,
        ttl_seconds: u64,
    ) -> Result<Value, TyrionError> {
        validate_attachment_identity(expected_adapter)?;
        if ttl_seconds == 0 || ttl_seconds > 300 {
            return Err(TyrionError::InvalidRequest(
                "attachment token TTL must be between 1 and 300 seconds".into(),
            ));
        }
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }

        let launch_token = format!("tlt_{}_{}", Uuid::new_v4(), Uuid::new_v4());
        let token_hash = attachment_token_hash(&launch_token);
        let created_at = unix_timestamp()?;
        let expires_at = created_at.saturating_add(ttl_seconds as i64);
        transaction.execute(
            "INSERT INTO attachment_launch_tokens (
                token_hash, expected_harness, expected_adapter_identity,
                expected_adapter_version, created_at, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                token_hash,
                expected_adapter.harness,
                expected_adapter.adapter_identity,
                expected_adapter.adapter_version,
                created_at,
                expires_at,
            ],
        )?;
        let result = serde_json::json!({
            "launch_token": launch_token,
            "expected_harness": expected_adapter.harness,
            "expected_adapter_identity": expected_adapter.adapter_identity,
            "expected_adapter_version": expected_adapter.adapter_version,
            "created_at": created_at,
            "expires_at": expires_at,
        });
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn connect_attachment(
        &mut self,
        request: &Request,
        launch_token: &str,
        handshake: &AttachmentHandshake,
        replay_cursor: Option<&CommissionReplayCursor>,
    ) -> Result<Value, TyrionError> {
        validate_attachment_identity(&handshake.adapter)?;
        if handshake.native_session_id.trim().is_empty() {
            return Err(TyrionError::AttachmentRejected(
                "native session identity must not be empty".into(),
            ));
        }
        if replay_cursor.is_some_and(|cursor| cursor.last_event_sequence < 0) {
            return Err(TyrionError::AttachmentRejected(
                "last durable event cursor must not be negative".into(),
            ));
        }
        if handshake.adapter_protocol_version != PROTOCOL_VERSION {
            return Err(TyrionError::AttachmentRejected(format!(
                "adapter protocol version {} is incompatible with {PROTOCOL_VERSION}",
                handshake.adapter_protocol_version
            )));
        }
        let negotiated = attachment::negotiate(&handshake.capabilities)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let transaction = self.connection.transaction()?;
        if let Some(cursor) = replay_cursor {
            ensure_commission_exists(&transaction, &cursor.commission_id)?;
        }

        let token_hash = attachment_token_hash(launch_token);
        let token = transaction
            .query_row(
                "SELECT expected_harness, expected_adapter_identity, expected_adapter_version,
                        expires_at, consumed_at
                 FROM attachment_launch_tokens WHERE token_hash = ?1",
                [&token_hash],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::AttachmentRejected("launch token is invalid".into()))?;
        if token.4.is_some() {
            return Err(TyrionError::AttachmentRejected(
                "launch token was already used".into(),
            ));
        }
        let now = unix_timestamp()?;
        if now >= token.3 {
            return Err(TyrionError::AttachmentRejected(
                "launch token has expired".into(),
            ));
        }
        if handshake.adapter.harness != token.0 {
            return Err(TyrionError::AttachmentRejected(format!(
                "harness identity mismatch: expected {}",
                token.0
            )));
        }
        if handshake.adapter.adapter_identity != token.1 {
            return Err(TyrionError::AttachmentRejected(format!(
                "adapter identity mismatch: expected {}",
                token.1
            )));
        }
        if handshake.adapter.adapter_version != token.2 {
            return Err(TyrionError::AttachmentRejected(format!(
                "adapter version mismatch: expected {}",
                token.2
            )));
        }
        if prior_result(&transaction, idempotency_key, &request_hash)?.is_some() {
            return Err(TyrionError::AttachmentRejected(
                "launch token was already used".into(),
            ));
        }

        let attachment_id = Uuid::new_v4().to_string();
        let attachment_session_token = format!("tat_{}_{}", Uuid::new_v4(), Uuid::new_v4());
        let session_token_hash = attachment_token_hash(&attachment_session_token);
        transaction.execute(
            "INSERT INTO attachments (
                id, session_token_hash, harness, adapter_identity, adapter_version,
                protocol_version, native_session_id, mode, capabilities_json,
                missing_capabilities_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                attachment_id,
                session_token_hash,
                handshake.adapter.harness,
                handshake.adapter.adapter_identity,
                handshake.adapter.adapter_version,
                handshake.adapter_protocol_version,
                handshake.native_session_id,
                negotiated.mode.as_str(),
                serde_json::to_string(&negotiated.effective)?,
                serde_json::to_string(&negotiated.missing)?,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE attachment_launch_tokens SET consumed_at = ?2 WHERE token_hash = ?1",
            params![token_hash, now],
        )?;
        if let Some(cursor) = replay_cursor {
            let commission_id = &cursor.commission_id;
            transaction.execute(
                "INSERT INTO commission_attachments (commission_id, attachment_id, role, joined_at)
                 VALUES (?1, ?2, 'observer', ?3)",
                params![commission_id, attachment_id, now],
            )?;
            let commission_revision = transaction.query_row(
                "SELECT revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| row.get::<_, i64>(0),
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::AttachmentJoined,
                commission_revision,
                &serde_json::json!({
                    "attachment_id": attachment_id,
                    "role": "observer",
                }),
            )?;
        }
        let replay = if negotiated.effective.contains(&attachment::EVENT_REPLAY) {
            replay_cursor
                .map(|cursor| {
                    replay_events(
                        &transaction,
                        &attachment_id,
                        &cursor.commission_id,
                        cursor.last_event_sequence,
                    )
                })
                .transpose()?
        } else {
            None
        };
        let result = serde_json::json!({
            "attachment": {
                "id": attachment_id,
                "harness": handshake.adapter.harness,
                "adapter_identity": handshake.adapter.adapter_identity,
                "adapter_version": handshake.adapter.adapter_version,
                "protocol_version": handshake.adapter_protocol_version,
                "native_session_id": handshake.native_session_id,
                "mode": negotiated.mode.as_str(),
                "mode_tag": negotiated.mode.tag(),
                "capabilities": negotiated.effective,
                "missing_capabilities": negotiated.missing,
            },
            "commission_id": replay_cursor.map(|cursor| &cursor.commission_id),
            "commission_role": replay_cursor.map(|_| "observer"),
            "replay": replay,
            "attachment_session_token": attachment_session_token,
        });
        let persisted_result = serde_json::json!({
            "attachment_id": attachment_id,
            "attachment_handshake": "consumed",
        });
        save_idempotent_result(
            &transaction,
            idempotency_key,
            &request_hash,
            &persisted_result,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn resume_attachment(
        &self,
        request: &Request,
        handshake: &AttachmentHandshake,
        replay_cursor: &CommissionReplayCursor,
    ) -> Result<Value, TyrionError> {
        validate_attachment_identity(&handshake.adapter)?;
        if handshake.native_session_id.trim().is_empty() {
            return Err(TyrionError::AttachmentRejected(
                "native session identity must not be empty".into(),
            ));
        }
        if replay_cursor.last_event_sequence < 0 {
            return Err(TyrionError::AttachmentRejected(
                "last durable event cursor must not be negative".into(),
            ));
        }
        if handshake.adapter_protocol_version != PROTOCOL_VERSION {
            return Err(TyrionError::AttachmentRejected(format!(
                "adapter protocol version {} is incompatible with {PROTOCOL_VERSION}",
                handshake.adapter_protocol_version
            )));
        }
        let negotiated = attachment::negotiate(&handshake.capabilities)?;
        let attachment_id = authenticated_attachment_id(&self.connection, request)?;
        let stored = self.connection.query_row(
            "SELECT harness, adapter_identity, adapter_version, protocol_version,
                    native_session_id, mode, capabilities_json
             FROM attachments WHERE id = ?1",
            [&attachment_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )?;
        let stored_capabilities = serde_json::from_str::<Vec<String>>(&stored.6)?;
        let negotiated_capabilities = negotiated
            .effective
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect::<Vec<_>>();
        if stored.0 != handshake.adapter.harness
            || stored.1 != handshake.adapter.adapter_identity
            || stored.2 != handshake.adapter.adapter_version
            || stored.3 != handshake.adapter_protocol_version
            || stored.4 != handshake.native_session_id
            || stored.5 != negotiated.mode.as_str()
            || stored_capabilities != negotiated_capabilities
        {
            return Err(TyrionError::AttachmentRejected(
                "resume handshake does not match the authenticated Attachment".into(),
            ));
        }
        ensure_commission_attachment(
            &self.connection,
            &attachment_id,
            &replay_cursor.commission_id,
            attachment::EVENT_REPLAY,
        )?;
        let replay = replay_events(
            &self.connection,
            &attachment_id,
            &replay_cursor.commission_id,
            replay_cursor.last_event_sequence,
        )?;
        Ok(serde_json::json!({
            "attachment_id": attachment_id,
            "resumed": true,
            "replay": replay,
        }))
    }

    pub fn inspect_commission(
        &self,
        request: &Request,
        commission_id: &str,
        runtime: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        let attachment_id = authenticated_attachment_id(&self.connection, request)?;
        ensure_commission_attachment(
            &self.connection,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        let mut projection = project_commission(&self.connection, commission_id)?;
        apply_attachment_worker_controls(
            &self.connection,
            &attachment_id,
            commission_id,
            &mut projection,
            runtime,
        )?;
        Ok(projection)
    }

    pub fn replay_events(
        &self,
        request: &Request,
        commission_id: &str,
        after_sequence: i64,
    ) -> Result<Value, TyrionError> {
        if after_sequence < 0 {
            return Err(TyrionError::InvalidRequest(
                "event replay cursor must not be negative".into(),
            ));
        }
        let attachment_id = authenticated_attachment_id(&self.connection, request)?;
        ensure_commission_attachment(
            &self.connection,
            &attachment_id,
            commission_id,
            attachment::EVENT_REPLAY,
        )?;
        replay_events(
            &self.connection,
            &attachment_id,
            commission_id,
            after_sequence,
        )
    }

    pub fn ready_commission_ids(&self) -> Result<Vec<String>, TyrionError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT commission_id FROM assignments
             WHERE status IN (?1, ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM attempts
                   JOIN sandbox_cleanups ON sandbox_cleanups.attempt_id = attempts.id
                   WHERE attempts.assignment_id = assignments.id
               )
             ORDER BY commission_id",
        )?;
        let rows = statement.query_map(
            params![
                AssignmentStatus::Ready.as_str(),
                AssignmentStatus::AttentionRequired.as_str()
            ],
            |row| row.get(0),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn recover_stranded_operations(
        &mut self,
        credential_runtime: Option<&CredentialRuntime>,
    ) -> Result<(), TyrionError> {
        let stranded = {
            let mut statement = self.connection.prepare(
                "SELECT operation_requests.id, operation_requests.commission_id,
                        operation_requests.operation_digest, commissions.revision,
                        operation_execution_identities.idempotency_key,
                        operation_execution_identities.request_hash,
                        operation_requests.canonical_operation_json,
                        operation_requests.credential_process_id,
                        operation_requests.credential_process_marker,
                        operation_requests.credential_process_status
                 FROM operation_requests
                 JOIN commissions ON commissions.id = operation_requests.commission_id
                 LEFT JOIN operation_execution_identities
                   ON operation_execution_identities.operation_request_id = operation_requests.id
                 WHERE operation_requests.status = 'started'
                 ORDER BY operation_requests.started_at, operation_requests.id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<u32>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut recovered = Vec::with_capacity(stranded.len());
        for (
            operation_request_id,
            commission_id,
            operation_digest,
            revision,
            idempotency_key,
            request_hash,
            canonical_json,
            credential_process_id,
            credential_process_marker,
            credential_process_status,
        ) in stranded
        {
            let cleanup = (|| -> Result<Option<Value>, TyrionError> {
                let mut canonical: Value = serde_json::from_str(&canonical_json)?;
                canonical
                    .as_object_mut()
                    .ok_or_else(|| {
                        TyrionError::InvalidRequest(
                            "stranded canonical operation must be an object".into(),
                        )
                    })?
                    .remove("target_revision");
                let operation: OperationRequest = serde_json::from_value(canonical)?;
                let Some(credential_use) = operation.credential.as_ref() else {
                    return Ok(None);
                };
                let credential_reference = self
                    .connection
                    .query_row(
                        "SELECT credential_reference FROM credential_grants WHERE id = ?1",
                        [&credential_use.grant_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        TyrionError::ControlDenied(
                            "stranded effect has no durable Credential Grant".into(),
                        )
                    })?;
                let runtime = credential_runtime.ok_or_else(|| {
                    TyrionError::ControlDenied(
                        "stranded credential cleanup requires its pinned runtime".into(),
                    )
                })?;
                runtime
                    .recover_stranded_effect(
                        &operation_request_id,
                        &operation,
                        &credential_reference,
                        active_broker_process(
                            credential_process_id,
                            credential_process_marker.as_deref(),
                            credential_process_status.as_deref(),
                        )?,
                    )
                    .map(Some)
            })()
            .map_err(|error| error.to_string());
            recovered.push(StrandedOperationRecovery {
                operation_request_id,
                commission_id,
                operation_digest,
                revision,
                idempotency_key,
                request_hash,
                cleanup,
            });
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recovered_at = unix_timestamp()?;
        let mut replay_results = Vec::new();
        for stranded in recovered {
            let (cleanup_receipt, containment_confirmed, requirement) = match stranded.cleanup {
                Ok(cleanup) => (
                    cleanup,
                    true,
                    "Reconcile the exact target revision before any linked Commission retries it."
                        .to_owned(),
                ),
                Err(error) => (
                    None,
                    false,
                    format!(
                        "Complete exact credential and Effect Sandbox cleanup before read-only reconciliation: {error}"
                    ),
                ),
            };
            let receipt = serde_json::json!({
                "status": "uncertain",
                "effect_may_have_occurred": true,
                "rollback_claimed": false,
                "containment_confirmed": containment_confirmed,
                "cleanup": cleanup_receipt,
                "requirement": requirement,
                "recovered_after_control_plane_restart": true,
            });
            if containment_confirmed {
                transaction.execute(
                    "UPDATE operation_requests SET credential_process_status = 'contained'
                     WHERE id = ?1 AND credential_process_status = 'active'",
                    [&stranded.operation_request_id],
                )?;
            }
            transaction.execute(
                "UPDATE operation_requests
                 SET status = 'uncertain', completed_at = ?2, receipt_json = ?3
                 WHERE id = ?1 AND status = 'started'",
                params![
                    stranded.operation_request_id,
                    recovered_at,
                    serde_json::to_string(&receipt)?,
                ],
            )?;
            record_event_with_payload(
                &transaction,
                &stranded.commission_id,
                EventKind::OperationUncertain,
                stranded.revision,
                &serde_json::json!({
                    "operation_request_id": stranded.operation_request_id,
                    "operation_digest": stranded.operation_digest,
                    "receipt": receipt,
                }),
            )?;
            let paused = transaction.execute(
                "UPDATE commissions SET status = 'paused'
                 WHERE id = ?1 AND status = 'active'",
                [&stranded.commission_id],
            )?;
            if paused == 1 {
                record_event_with_payload(
                    &transaction,
                    &stranded.commission_id,
                    EventKind::CommissionPaused,
                    stranded.revision,
                    &serde_json::json!({
                        "cause": "uncertain_consequential_effect",
                        "operation_request_id": stranded.operation_request_id,
                        "dispatch_enabled": false,
                        "resumable_after_principal_reconciliation": containment_confirmed,
                    }),
                )?;
            }
            if let (Some(idempotency_key), Some(request_hash)) =
                (stranded.idempotency_key, stranded.request_hash)
            {
                replay_results.push((stranded.commission_id, idempotency_key, request_hash));
            }
        }
        for (commission_id, idempotency_key, request_hash) in replay_results {
            let result = project_commission(&transaction, &commission_id)?;
            save_idempotent_result(&transaction, &idempotency_key, &request_hash, &result)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn recover_stranded_attempts(&mut self) -> Result<Vec<PendingCleanup>, TyrionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stranded = {
            let mut statement = transaction.prepare(
                "SELECT attempts.id, attempts.assignment_id, assignments.commission_id,
                        commissions.revision, worker_leases.expires_at,
                        commissions.execution_json,
                        EXISTS(
                            SELECT 1 FROM results
                            WHERE results.attempt_id = attempts.id
                              AND commissions.artifact_revision IS NOT NULL
                              AND results.integrated_artifact_revision = commissions.artifact_revision
                        )
                 FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 JOIN commissions ON commissions.id = assignments.commission_id
                 JOIN worker_leases ON worker_leases.attempt_id = attempts.id
                 WHERE attempts.status = 'running'
                   AND commissions.status IN ('active', 'paused')
                 ORDER BY attempts.started_at, attempts.id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let now = unix_timestamp()?;
        let now_ms = unix_timestamp_millis()?;
        for (
            attempt_id,
            assignment_id,
            commission_id,
            revision,
            lease_expires_at,
            execution_json,
            acknowledged_state,
        ) in &stranded
        {
            transaction.execute(
                "INSERT OR IGNORE INTO sandbox_cleanups (attempt_id, created_at)
                 VALUES (?1, ?2)",
                params![attempt_id, now],
            )?;
            transaction.execute(
                "UPDATE attempts
                 SET status = 'failed', completed_at = ?2,
                     execution_completed_at_ms = COALESCE(execution_completed_at_ms, ?3),
                     completed_at_ms = ?3, revision_disposition = ?4
                 WHERE id = ?1 AND status = 'running'",
                params![
                    attempt_id,
                    now,
                    now_ms,
                    if *acknowledged_state {
                        "requires_revalidation"
                    } else {
                        "retained"
                    }
                ],
            )?;
            record_attempt_profile_claim_outcome(
                &transaction,
                attempt_id,
                ProfileClaimOutcome::Rejected,
            )?;
            transaction.execute(
                "UPDATE worker_leases
                 SET status = 'expired', released_at = ?2
                 WHERE attempt_id = ?1 AND status = 'active'",
                params![attempt_id, now],
            )?;
            transaction.execute(
                "UPDATE resource_reservations
                 SET status = 'revoked', released_at = ?2
                 WHERE attempt_id = ?1 AND status = 'active'",
                params![attempt_id, now_ms],
            )?;
            transaction.execute(
                "UPDATE workers
                 SET status = 'failed', latest_activity = ?2, activity_at_ms = ?3
                 WHERE attempt_id = ?1 AND status = 'running'",
                params![
                    attempt_id,
                    "Worker lost during Control Plane restart",
                    now_ms
                ],
            )?;
            if *acknowledged_state {
                transaction.execute(
                    "UPDATE results
                     SET status = 'candidate', revision_disposition = 'requires_revalidation'
                     WHERE attempt_id = ?1 AND integrated_artifact_revision = (
                         SELECT artifact_revision FROM commissions WHERE id = ?2
                     )",
                    params![attempt_id, commission_id],
                )?;
                transaction.execute(
                    "UPDATE results
                     SET status = 'superseded', revision_disposition = 'retained'
                     WHERE attempt_id = ?1 AND status != 'accepted'
                       AND integrated_artifact_revision IS NOT (
                           SELECT artifact_revision FROM commissions WHERE id = ?2
                       )",
                    params![attempt_id, commission_id],
                )?;
            } else {
                transaction.execute(
                    "UPDATE results
                     SET status = 'superseded', revision_disposition = 'retained'
                     WHERE attempt_id = ?1 AND status != 'accepted'",
                    [attempt_id],
                )?;
            }
            transaction.execute(
                "UPDATE worker_commands
                 SET status = 'failed'
                 WHERE worker_id IN (
                     SELECT id FROM workers WHERE attempt_id = ?1
                 ) AND status = 'pending'",
                [attempt_id],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO watchdog_findings (
                    id, commission_id, assignment_id, attempt_id, signal, action, details, created_at
                 ) VALUES (?1, ?2, ?3, ?4, 'lost_liveness', 'contain_attempt', ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    attempt_id,
                    "Control Plane restart found a durable running Attempt without a provable live Worker identity.",
                    now,
                ],
            )?;
            let attempt_count = transaction.query_row(
                "SELECT COUNT(*) FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1",
                [commission_id],
                |row| row.get::<_, u32>(0),
            )?;
            let max_attempts = transaction.query_row(
                "SELECT max_attempts FROM resource_ceilings WHERE commission_id = ?1",
                [commission_id],
                |row| row.get::<_, u32>(0),
            )?;
            let prior_equivalent_failures = transaction.query_row(
                "SELECT COUNT(*) FROM attempt_recoveries
                 WHERE assignment_id = ?1 AND equivalence_key = 'lost_liveness'",
                [assignment_id],
                |row| row.get::<_, u32>(0),
            )?;
            let execution: ExecutionSpec = serde_json::from_str(execution_json)?;
            let current_authority =
                attempt_authority_is_current(&transaction, commission_id, attempt_id, &execution)?;
            let within_attempt_ceiling = attempt_count < max_attempts;
            let retry_safe = !*acknowledged_state
                && current_authority
                && within_attempt_ceiling
                && prior_equivalent_failures == 0;
            let replan_required = !*acknowledged_state
                && current_authority
                && within_attempt_ceiling
                && prior_equivalent_failures > 0;
            let decision = if retry_safe {
                "expire_and_retry"
            } else if replan_required {
                "expire_and_replan"
            } else {
                "expire_and_block"
            };
            let requirement = if *acknowledged_state {
                "Revalidate the retained integrated Result against the current mandate before any further execution."
            } else if retry_safe {
                "Retry only after sandbox cleanup confirms containment; native reattachment was not proven."
            } else if replan_required {
                "Revise the Assignment and use a different eligible Worker Configuration; a second equivalent lost-liveness failure exhausted same-configuration retry."
            } else if !current_authority {
                "Restore the currently required execution authority in a linked Commission; restart reconciliation could not prove current authority."
            } else {
                "Increase max_attempts in a linked Commission; native reattachment was not proven."
            };
            let assignment_status = if *acknowledged_state {
                AssignmentStatus::VerificationFailed
            } else if retry_safe {
                AssignmentStatus::Ready
            } else if replan_required {
                AssignmentStatus::AttentionRequired
            } else {
                AssignmentStatus::ResourceBlocked
            };
            transaction.execute(
                "UPDATE assignments SET status = ?2 WHERE id = ?1 AND status = 'running'",
                params![assignment_id, assignment_status.as_str()],
            )?;
            transaction.execute(
                "INSERT OR REPLACE INTO restart_recoveries (
                    attempt_id, commission_id, decision, process_identity,
                    native_session_identity, acknowledged_state, lease_validity,
                    current_authority, containment, cleanup_confirmed, requirement, created_at
                 ) VALUES (?1, ?2, ?3, 0, 0, ?4, ?5, ?6, 0, 0, ?7, ?8)",
                params![
                    attempt_id,
                    commission_id,
                    decision,
                    acknowledged_state,
                    *lease_expires_at > now,
                    current_authority,
                    requirement,
                    now,
                ],
            )?;
            if replan_required {
                let next_plan_revision = transaction.query_row(
                    "SELECT COALESCE(MAX(revision), 0) + 1 FROM commission_plans
                     WHERE commission_id = ?1",
                    [commission_id],
                    |row| row.get::<_, i64>(0),
                )?;
                let snapshot = serde_json::json!({
                    "reason": "second_equivalent_restart_failure",
                    "assignment_id": assignment_id,
                    "attempt_id": attempt_id,
                    "equivalence_key": "lost_liveness",
                    "requirement": requirement,
                });
                transaction.execute(
                    "INSERT INTO commission_plans (
                        commission_id, revision, source, reason, snapshot_json, created_at
                     ) VALUES (?1, ?2, 'control_plane',
                               'second equivalent restart failure', ?3, ?4)",
                    params![
                        commission_id,
                        next_plan_revision,
                        serde_json::to_string(&snapshot)?,
                        now,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO attention_conditions (
                        id, commission_id, assignment_id, code, requirement, status, created_at
                     ) VALUES (?1, ?2, ?3, 'replan_required', ?4, 'open', ?5)",
                    params![
                        Uuid::new_v4().to_string(),
                        commission_id,
                        assignment_id,
                        requirement,
                        now,
                    ],
                )?;
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::PlanRevised,
                    *revision,
                    &serde_json::json!({
                        "plan_revision": next_plan_revision,
                        "reason": "second_equivalent_restart_failure",
                        "assignment_id": assignment_id,
                    }),
                )?;
            } else if !retry_safe {
                let blocker_code = if *acknowledged_state {
                    "integrated_revalidation"
                } else if current_authority {
                    "max_attempts"
                } else {
                    "current_authority"
                };
                transaction.execute(
                    "INSERT INTO blockers (
                        id, commission_id, assignment_id, code, requirement, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        Uuid::new_v4().to_string(),
                        commission_id,
                        assignment_id,
                        blocker_code,
                        requirement,
                        now,
                    ],
                )?;
            }
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: if *acknowledged_state {
                        "acknowledged_integrated_state"
                    } else {
                        "lost_liveness"
                    },
                    classification: if *acknowledged_state {
                        "repairable_context"
                    } else if current_authority {
                        "transient"
                    } else {
                        "authority"
                    },
                    equivalence_key: if *acknowledged_state {
                        "integrated_revalidation"
                    } else {
                        "lost_liveness"
                    },
                    action: if retry_safe {
                        "retry"
                    } else if replan_required {
                        "replan"
                    } else {
                        "block"
                    },
                    requirement,
                },
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::RestartReconciled,
                *revision,
                &serde_json::json!({
                    "attempt_id": attempt_id,
                    "decision": decision,
                    "all_reattachment_proofs_satisfied": false,
                    "cleanup_required": true,
                }),
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::WorkerActivity,
                *revision,
                &serde_json::json!({
                    "assignment_id": assignment_id,
                    "attempt_id": attempt_id,
                    "activity": "Worker lost during Control Plane restart",
                    "terminal_state": "failed",
                }),
            )?;
            if retry_safe {
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::AssignmentReady,
                    *revision,
                    &serde_json::json!({
                        "assignment_id": assignment_id,
                        "reason": "stranded_attempt_recovered",
                    }),
                )?;
            } else {
                record_event_with_payload(
                    &transaction,
                    commission_id,
                    EventKind::AssignmentBlocked,
                    *revision,
                    &serde_json::json!({
                        "assignment_id": assignment_id,
                        "reason": if replan_required {
                            "second_equivalent_restart_failure"
                        } else {
                            "restart_recovery_blocked"
                        },
                    }),
                )?;
            }
        }
        let pending_cleanups = {
            let mut statement = transaction.prepare(
                "SELECT sandbox_cleanups.attempt_id, assignments.commission_id,
                        commissions.execution_json, commissions.artifact_revision
                 FROM sandbox_cleanups
                 JOIN attempts ON attempts.id = sandbox_cleanups.attempt_id
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 JOIN commissions ON commissions.id = assignments.commission_id
                 ORDER BY sandbox_cleanups.created_at, sandbox_cleanups.attempt_id",
            )?;
            let rows = statement.query_map([], |row| {
                let execution_json = row.get::<_, String>(2)?;
                Ok(PendingCleanup {
                    attempt_id: row.get(0)?,
                    commission_id: row.get(1)?,
                    execution: serde_json::from_str(&execution_json)
                        .map_err(|error| invalid_json_column(2, error))?,
                    artifact_revision: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
        Ok(pending_cleanups)
    }

    pub fn complete_sandbox_cleanup(&mut self, attempt_id: &str) -> Result<(), TyrionError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM sandbox_cleanups WHERE attempt_id = ?1",
            [attempt_id],
        )?;
        transaction.execute(
            "UPDATE restart_recoveries SET cleanup_confirmed = 1 WHERE attempt_id = ?1",
            [attempt_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn watchdog_sweep(
        &mut self,
        runtime: &worker::WorkerRuntime,
        stall_milliseconds: u64,
    ) -> Result<Vec<String>, TyrionError> {
        let now = unix_timestamp()?;
        let now_ms = unix_timestamp_millis()?;
        let running = {
            let mut statement = self.connection.prepare(
                "SELECT attempts.id, attempts.assignment_id, assignments.commission_id,
                        commissions.revision, workers.activity_at_ms, worker_leases.id,
                        worker_leases.expires_at,
                        resource_reservations.concurrency_slots,
                        resource_reservations.storage_bytes,
                        resource_ceilings.max_worker_concurrency,
                        resource_ceilings.max_storage_bytes, commissions.execution_json
                 FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 JOIN commissions ON commissions.id = assignments.commission_id
                 JOIN workers ON workers.attempt_id = attempts.id
                 JOIN worker_leases ON worker_leases.attempt_id = attempts.id
                 JOIN resource_reservations ON resource_reservations.attempt_id = attempts.id
                 JOIN resource_ceilings ON resource_ceilings.commission_id = commissions.id
                 WHERE attempts.status = 'running' AND commissions.status IN ('active', 'paused')
                 ORDER BY attempts.started_at_ms DESC, attempts.id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, u32>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, u32>(9)?,
                    row.get::<_, u64>(10)?,
                    row.get::<_, String>(11)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut resource_containment_started = HashSet::new();
        let mut redispatch_commissions = HashSet::new();
        for (
            attempt_id,
            assignment_id,
            commission_id,
            revision,
            persisted_activity_at_ms,
            lease_id,
            _lease_expires_at,
            _reserved_concurrency,
            _reserved_storage,
            max_concurrency,
            max_storage,
            execution_json,
        ) in running
        {
            let execution: ExecutionSpec = serde_json::from_str(&execution_json)?;
            let authority_valid = attempt_authority_is_current(
                &self.connection,
                &commission_id,
                &attempt_id,
                &execution,
            )?;
            let retry_pattern = self.connection.query_row(
                "SELECT COALESCE(MAX(repeats), 0) FROM (
                    SELECT COUNT(*) AS repeats FROM attempt_recoveries
                    WHERE commission_id = ?1 AND assignment_id = ?2
                    GROUP BY equivalence_key
                 )",
                params![commission_id, assignment_id],
                |row| row.get::<_, u32>(0),
            )?;
            let (active_concurrency, active_storage) = self.connection.query_row(
                "SELECT COALESCE(SUM(concurrency_slots), 0), COALESCE(SUM(storage_bytes), 0)
                 FROM resource_reservations
                 WHERE commission_id = ?1 AND status = 'active'",
                [&commission_id],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?)),
            )?;
            let repeated_verification = self.connection.query_row(
                "SELECT COUNT(*) FROM evidence
                 JOIN attempts ON attempts.id = evidence.producer_attempt_id
                 WHERE evidence.commission_id = ?1 AND attempts.assignment_id = ?2
                   AND evidence.outcome = 'failed'",
                params![commission_id, assignment_id],
                |row| row.get::<_, u32>(0),
            )?;
            let live_activity_at_ms = runtime
                .live_telemetry(&attempt_id)
                .and_then(|telemetry| telemetry["activity_at_ms"].as_i64());
            let latest_activity_at_ms = live_activity_at_ms
                .unwrap_or(persisted_activity_at_ms)
                .max(persisted_activity_at_ms);
            let (signal, details) = if !authority_valid {
                (
                    Some("invalid_authority"),
                    "the current repository, path, action, or effect authority no longer covers the Attempt"
                        .to_owned(),
                )
            } else if active_concurrency > max_concurrency || active_storage > max_storage {
                if resource_containment_started.insert(commission_id.clone()) {
                    (
                        Some("abnormal_resource_use"),
                        "the newest active reservation pushed aggregate use beyond the Commission resource ceiling"
                            .to_owned(),
                    )
                } else {
                    (None, String::new())
                }
            } else if !runtime.is_attempt_active(&attempt_id) {
                (
                    Some("lost_liveness"),
                    "the durable running Attempt has no live Worker control identity".to_owned(),
                )
            } else if now_ms.saturating_sub(latest_activity_at_ms)
                >= i64::try_from(stall_milliseconds.max(1)).unwrap_or(i64::MAX)
            {
                (
                    Some("stall"),
                    "meaningful Worker activity stopped before a safe terminal state".to_owned(),
                )
            } else if retry_pattern >= 2 {
                (
                    Some("unhealthy_retry_pattern"),
                    "an equivalent recovery was attempted more than once".to_owned(),
                )
            } else if repeated_verification >= 2 {
                (
                    Some("repeated_verification_failure"),
                    "the Assignment produced repeated failed verification Evidence".to_owned(),
                )
            } else {
                (None, String::new())
            };
            let Some(signal) = signal else {
                continue;
            };
            let fence_transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let still_running = fence_transaction.query_row(
                "SELECT status = 'running' FROM attempts WHERE id = ?1",
                [&attempt_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !still_running {
                fence_transaction.commit()?;
                continue;
            }
            fence_transaction.execute(
                "INSERT OR IGNORE INTO sandbox_cleanups (attempt_id, created_at)
                 VALUES (?1, ?2)",
                params![attempt_id, now],
            )?;
            fence_transaction.commit()?;
            let control_delivery = runtime.watchdog_contain(&attempt_id, signal);
            let transaction = self.connection.transaction()?;
            transaction.execute(
                "INSERT OR IGNORE INTO watchdog_findings (
                    id, commission_id, assignment_id, attempt_id, signal, action, details, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'contain_attempt', ?6, ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    attempt_id,
                    signal,
                    if let Err(error) = &control_delivery {
                        format!("{details}; live control delivery reported: {error}")
                    } else {
                        details
                    },
                    now,
                ],
            )?;
            record_event_with_payload(
                &transaction,
                &commission_id,
                EventKind::AttemptContained,
                revision,
                &serde_json::json!({
                    "assignment_id": assignment_id,
                    "attempt_id": attempt_id,
                    "signal": signal,
                    "scope": "attempt",
                }),
            )?;
            transaction.commit()?;
            self.fail_attempt(
                &commission_id,
                &assignment_id,
                &attempt_id,
                &lease_id,
                revision,
                &TyrionError::WatchdogContained { signal },
            )?;
            let integration_lock = runtime.commission_integration_lock(&commission_id)?;
            let integration_guard = integration_lock.lock().map_err(|_| {
                TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
            })?;
            let artifact_revision = self.connection.query_row(
                "SELECT artifact_revision FROM commissions WHERE id = ?1",
                [&commission_id],
                |row| row.get::<_, Option<String>>(0),
            )?;
            runtime.cleanup_stranded_attempt(
                &attempt_id,
                &commission_id,
                &execution,
                artifact_revision.as_deref(),
            )?;
            self.complete_sandbox_cleanup(&attempt_id)?;
            drop(integration_guard);
            let redispatchable = self.connection.query_row(
                "SELECT assignments.status = 'ready' AND commissions.status = 'active'
                 FROM assignments
                 JOIN commissions ON commissions.id = assignments.commission_id
                 WHERE assignments.id = ?1 AND commissions.id = ?2",
                params![assignment_id, commission_id],
                |row| row.get::<_, bool>(0),
            )?;
            if redispatchable {
                redispatch_commissions.insert(commission_id);
            }
        }
        Ok(redispatch_commissions.into_iter().collect())
    }

    pub fn dispatch_snapshot(&self, commission_id: &str) -> Result<(u32, u32, u32), TyrionError> {
        self.connection
            .query_row(
                "SELECT resource_ceilings.max_worker_concurrency,
                    (SELECT COUNT(*) FROM assignments
                     WHERE assignments.commission_id = commissions.id
                       AND assignments.status IN (?2, ?3)),
                    (SELECT COUNT(*) FROM attempts
                     JOIN assignments ON assignments.id = attempts.assignment_id
                     WHERE assignments.commission_id = commissions.id)
             FROM commissions
             JOIN resource_ceilings ON resource_ceilings.commission_id = commissions.id
             WHERE commissions.id = ?1",
                params![
                    commission_id,
                    AssignmentStatus::Ready.as_str(),
                    AssignmentStatus::AttentionRequired.as_str()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(Into::into)
    }

    pub fn accept_commission(
        &mut self,
        request: &Request,
        commission_id: &str,
        worker: &worker::WorkerRuntime,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "commission acceptance requires an expected revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;

        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;

        let (status, current_revision, execution_json, plan_json) = transaction
            .query_row(
                "SELECT status, revision, execution_json, plan_json
                 FROM commissions WHERE id = ?1",
                [commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if current_revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if status != CommissionStatus::Proposed.as_str() {
            return Err(TyrionError::InvalidRequest(format!(
                "commission {commission_id} is already {}",
                status
            )));
        }
        let execution: ExecutionSpec = serde_json::from_str(&execution_json)?;
        let required_action = match execution {
            ExecutionSpec::Deterministic => worker::DETERMINISTIC_ACTION,
            ExecutionSpec::CodexGit { .. } => worker::CODEX_GIT_ACTION,
        };
        let may_execute = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM authority_scopes
                WHERE commission_id = ?1 AND scope_type = ?2 AND value = ?3
             )",
            params![
                commission_id,
                AuthorityScopeType::Action.as_str(),
                required_action
            ],
            |row| row.get::<_, bool>(0),
        )?;
        if !may_execute {
            return Err(TyrionError::InvalidRequest(format!(
                "the Authority Envelope does not permit {required_action}"
            )));
        }

        let accepted_at = unix_timestamp()?;
        let mandate_revision = current_revision + 1;
        transaction.execute(
            "UPDATE commissions SET status = ?2, revision = ?3, accepted_at = ?4 WHERE id = ?1",
            params![
                commission_id,
                CommissionStatus::Active.as_str(),
                mandate_revision,
                accepted_at
            ],
        )?;
        transaction.execute(
            "UPDATE criterion_versions SET mandate_revision = ?2
             WHERE commission_id = ?1 AND mandate_revision = 0",
            params![commission_id, mandate_revision],
        )?;
        open_principal_verification_gates(
            &transaction,
            commission_id,
            mandate_revision,
            accepted_at,
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::CommissionAccepted,
            mandate_revision,
        )?;

        let plan = plan_json
            .as_deref()
            .map(serde_json::from_str::<CommissionPlan>)
            .transpose()?;
        let legacy = plan.is_none();
        let plan = proposal_plan_or_legacy(plan, commission_id, &transaction)?;
        initialize_commission_plan(
            &transaction,
            commission_id,
            mandate_revision,
            &plan,
            legacy,
            accepted_at,
        )?;
        route_ready_assignments(&transaction, commission_id, worker)?;

        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn amend_verification(
        &mut self,
        request: &Request,
        commission_id: &str,
        amendment: &VerificationAmendment,
    ) -> Result<Value, TyrionError> {
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "verification amendment requires an expected revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;
        let (status, current_revision, execution_json) = transaction
            .query_row(
                "SELECT status, revision, execution_json FROM commissions WHERE id = ?1",
                [commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if current_revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if status != CommissionStatus::Active.as_str() {
            return Err(TyrionError::InvalidRequest(format!(
                "Commission {commission_id} is {status}; only an active mandate can be amended"
            )));
        }
        let execution: ExecutionSpec = serde_json::from_str(&execution_json)?;
        validate_acceptance_criteria(&execution, &amendment.criteria)?;

        let current_criterion_ids = {
            let mut statement = transaction
                .prepare("SELECT criterion_id FROM criteria WHERE commission_id = ?1")?;
            let rows = statement.query_map([commission_id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<HashSet<_>, _>>()?
        };
        let amended_criterion_ids = amendment
            .criteria
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect::<HashSet<_>>();
        if amended_criterion_ids != current_criterion_ids {
            return Err(TyrionError::InvalidRequest(
                "this verification amendment must preserve the current criterion identifiers"
                    .into(),
            ));
        }

        let (assignment_id, assignment_status) = transaction.query_row(
            "SELECT id, status FROM assignments
             WHERE commission_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
            [commission_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        if assignment_status != AssignmentStatus::VerificationPending.as_str()
            && assignment_status != AssignmentStatus::VerificationFailed.as_str()
        {
            return Err(TyrionError::InvalidRequest(
                "verification can be amended only after the current Attempt reaches a verification outcome"
                    .into(),
            ));
        }

        let next_revision = current_revision + 1;
        transaction.execute(
            "UPDATE commissions SET revision = ?2 WHERE id = ?1",
            params![commission_id, next_revision],
        )?;
        resolve_all_verification_recoveries(&transaction, commission_id)?;
        for (position, criterion) in amendment.criteria.iter().enumerate() {
            let (verifier_kind, expected) = verifier_storage(&criterion.verifier)?;
            transaction.execute(
                "UPDATE criteria SET
                    position = ?3, description = ?4, required_evidence = ?5,
                    verifier_type = ?6, verification_depth = ?7,
                    verifier_configuration = ?8, verification_environment = ?9,
                    verifier_kind = ?10, expected = ?11, status = ?12
                 WHERE commission_id = ?1 AND criterion_id = ?2",
                params![
                    commission_id,
                    criterion.id,
                    position as i64,
                    criterion.description,
                    criterion.required_evidence,
                    criterion.verifier_type.as_str(),
                    criterion.verification_depth.as_str(),
                    resolved_verifier_configuration(criterion),
                    criterion.verification_environment,
                    verifier_kind,
                    expected,
                    CriterionStatus::Uncertain.as_str(),
                ],
            )?;
            insert_criterion_version(
                &transaction,
                commission_id,
                next_revision,
                position,
                criterion,
            )?;
        }
        open_principal_verification_gates(
            &transaction,
            commission_id,
            next_revision,
            unix_timestamp()?,
        )?;
        transaction.execute(
            "UPDATE results SET status = ?2, revision_disposition = 'superseded'
             WHERE status = ?3 AND attempt_id IN (
                 SELECT id FROM attempts WHERE assignment_id = ?1
             )",
            params![
                assignment_id,
                ResultStatus::Superseded.as_str(),
                ResultStatus::Candidate.as_str()
            ],
        )?;
        transaction.execute(
            "UPDATE attempts SET revision_disposition = 'requires_revalidation'
             WHERE assignment_id = ?1 AND status = 'succeeded'",
            [&assignment_id],
        )?;
        transaction.execute(
            "UPDATE assignments SET plan_revision = ?2, status = ?3 WHERE id = ?1",
            params![
                assignment_id,
                next_revision,
                AssignmentStatus::Ready.as_str()
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::CommissionAmended,
            next_revision,
            &serde_json::json!({
                "previous_mandate_revision": current_revision,
                "mandate_revision": next_revision,
                "changed_section": "verification",
            }),
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::AssignmentReady,
            next_revision,
        )?;
        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn record_verification_evidence(
        &mut self,
        request: &Request,
        commission_id: &str,
        evidence: &VerificationEvidenceSubmission,
    ) -> Result<Value, TyrionError> {
        validate_evidence_submission(evidence)?;
        let idempotency_key = mutation_key(request)?;
        let request_hash = request_hash(request)?;
        let expected_revision = request.expected_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "recording verification Evidence requires an expected revision".into(),
            )
        })?;
        let transaction = self.connection.transaction()?;
        if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
            return Ok(prior);
        }
        let attachment_id = authenticated_attachment_id(&transaction, request)?;
        ensure_active_attachment(
            &transaction,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_ACCEPTANCE,
        )?;

        let (status, current_revision, artifact_revision) = transaction
            .query_row(
                "SELECT status, revision, artifact_revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if current_revision != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: current_revision,
            });
        }
        if status != CommissionStatus::Active.as_str() {
            return Err(TyrionError::InvalidRequest(format!(
                "Commission {commission_id} is {status}; Evidence can be recorded only while active"
            )));
        }
        let criterion = transaction
            .query_row(
                "SELECT criterion_id, required_evidence, verifier_type, verification_depth,
                        verifier_configuration, verification_environment,
                        verifier_kind, expected
                 FROM criteria WHERE commission_id = ?1 AND criterion_id = ?2",
                params![commission_id, evidence.criterion_id],
                stored_criterion,
            )
            .optional()?
            .ok_or_else(|| {
                TyrionError::InvalidRequest(format!(
                    "Acceptance Criterion {} was not found",
                    evidence.criterion_id
                ))
            })?;
        if criterion.verifier_type == VerifierType::Deterministic {
            return Err(TyrionError::InvalidRequest(
                "deterministic Evidence is recorded only by the Control Plane and cannot be overridden through an Entry Session".into(),
            ));
        }
        let artifact_revision = artifact_revision.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "verification Evidence requires a current integrated artifact".into(),
            )
        })?;
        let (submitted_kind, submitted_expected) = verifier_storage(&evidence.procedure)?;
        if evidence.evidence_type != criterion.required_evidence
            || evidence.verifier_configuration != criterion.verifier_configuration
            || evidence.environment != criterion.verification_environment
            || submitted_kind != criterion.verifier_kind
            || submitted_expected != criterion.expected
        {
            return Err(TyrionError::InvalidRequest(
                "Evidence does not match the current criterion, verifier configuration, procedure, or environment".into(),
            ));
        }

        let (producer_attempt_id, integrated_artifact_revision, result_status) = transaction
            .query_row(
                "SELECT attempts.id, results.integrated_artifact_revision, results.status
                     FROM results
                     JOIN attempts ON attempts.id = results.attempt_id
                     JOIN assignments ON assignments.id = attempts.assignment_id
                     WHERE results.id = ?1 AND assignments.commission_id = ?2",
                params![evidence.result_id, commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "Evidence Result does not belong to this Commission".into(),
                )
            })?;
        if integrated_artifact_revision.as_deref() != Some(artifact_revision.as_str()) {
            return Err(TyrionError::InvalidRequest(
                "Evidence Result is not the current integrated artifact".into(),
            ));
        }
        if result_status != ResultStatus::Candidate.as_str() {
            return Err(TyrionError::InvalidRequest(
                "Evidence Result is no longer the current candidate".into(),
            ));
        }
        let verification_attempt_id = Uuid::new_v4().to_string();
        let verifier_identity = format!("attachment:{attachment_id}");

        let evidence_id = Uuid::new_v4().to_string();
        resolve_verification_recoveries(&transaction, commission_id, &evidence.criterion_id)?;
        transaction.execute(
            "INSERT INTO evidence (
                id, commission_id, criterion_id, result_id, mandate_revision,
                artifact_revision, evidence_type, verifier_type, scope,
                verification_attempt_id, verifier_identity, verifier_configuration,
                verifier_kind, procedure_json, environment, outcome, observed, expected,
                material_contradiction, defect, producer_attempt_id, created_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'external', ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
             )",
            params![
                evidence_id,
                commission_id,
                evidence.criterion_id,
                evidence.result_id,
                current_revision,
                artifact_revision,
                evidence.evidence_type,
                criterion.verifier_type.as_str(),
                verification_attempt_id,
                verifier_identity,
                evidence.verifier_configuration,
                submitted_kind,
                serde_json::to_string(&evidence.procedure)?,
                evidence.environment,
                evidence.verdict.as_str(),
                evidence.inspectable_output,
                submitted_expected,
                evidence.material_contradiction,
                evidence.defect.map(|defect| defect.as_str()),
                producer_attempt_id,
                unix_timestamp()?,
            ],
        )?;
        if evidence.verdict != VerificationVerdict::Passed {
            let outcome = if evidence.material_contradiction {
                ProfileClaimOutcome::Contradicted
            } else {
                ProfileClaimOutcome::Rejected
            };
            record_result_profile_claim_outcome(&transaction, &evidence.result_id, outcome)?;
        }
        record_event(
            &transaction,
            commission_id,
            EventKind::EvidenceRecorded,
            current_revision,
        )?;
        refresh_criterion_statuses(
            &transaction,
            commission_id,
            current_revision,
            &artifact_revision,
        )?;
        refresh_principal_verification_gate(
            &transaction,
            commission_id,
            &criterion,
            current_revision,
        )?;
        let recovery_id = plan_verification_recovery(
            &transaction,
            commission_id,
            current_revision,
            &evidence_id,
            evidence,
        )?;
        route_result_rework(
            &transaction,
            commission_id,
            current_revision,
            evidence,
            recovery_id.as_deref(),
        )?;
        complete_after_external_verification(
            &transaction,
            commission_id,
            current_revision,
            &artifact_revision,
        )?;

        let result = project_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn run_ready_assignment(
        &mut self,
        commission_id: &str,
        worker: &worker::WorkerRuntime,
    ) -> Result<(), TyrionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        route_ready_assignments(&transaction, commission_id, worker)?;
        let ready_candidates = {
            let mut statement = transaction.prepare(
                "SELECT assignments.id, assignment_metadata.logical_id,
                        assignment_metadata.goal, assignments.plan_revision,
                        commissions.revision, commissions.accepted_at,
                        resource_ceilings.max_attempts,
                        resource_ceilings.max_elapsed_seconds,
                        resource_ceilings.max_worker_concurrency,
                        resource_ceilings.max_storage_bytes,
                        resource_ceilings.max_model_spend_cents,
                        resource_ceilings.max_paid_service_spend_cents,
                        commissions.execution_json, commissions.artifact_revision,
                        assignment_metadata.concurrency_slots,
                        assignment_metadata.max_storage_bytes,
                        assignment_metadata.max_model_spend_cents,
                        assignment_metadata.max_paid_service_spend_cents,
                        assignment_metadata.write_scopes_json,
                        assignment_metadata.competition_group,
                        assignment_metadata.competition_uncertainty,
                        assignment_metadata.competition_rule,
                        assignment_metadata.legacy,
                        assignment_metadata.purpose
                 FROM assignments
                 JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
                 JOIN commissions ON commissions.id = assignments.commission_id
                 JOIN resource_ceilings ON resource_ceilings.commission_id = commissions.id
                 WHERE assignments.commission_id = ?1
                   AND assignments.status = ?2
                   AND commissions.status = ?3
                   AND NOT EXISTS (
                       SELECT 1 FROM attempts AS cleanup_attempts
                       JOIN sandbox_cleanups
                         ON sandbox_cleanups.attempt_id = cleanup_attempts.id
                       WHERE cleanup_attempts.assignment_id = assignments.id
                   )
                   AND EXISTS (
                       SELECT 1 FROM assignment_routes
                       WHERE assignment_routes.assignment_id = assignments.id
                         AND assignment_routes.status = 'selected'
                   )
                 ORDER BY assignment_metadata.position, assignments.id",
            )?;
            let rows = statement.query_map(
                params![
                    commission_id,
                    AssignmentStatus::Ready.as_str(),
                    CommissionStatus::Active.as_str()
                ],
                |row| {
                    Ok(ReadyAssignmentDispatch {
                        assignment_id: row.get(0)?,
                        logical_id: row.get(1)?,
                        goal: row.get(2)?,
                        plan_revision: row.get(3)?,
                        mandate_revision: row.get(4)?,
                        accepted_at: row.get(5)?,
                        max_attempts: row.get(6)?,
                        max_elapsed_seconds: row.get(7)?,
                        max_worker_concurrency: row.get(8)?,
                        max_storage_bytes: row.get(9)?,
                        max_model_spend_cents: row.get(10)?,
                        max_paid_service_spend_cents: row.get(11)?,
                        execution_json: row.get(12)?,
                        current_artifact_revision: row.get(13)?,
                        reserved_concurrency_slots: row.get(14)?,
                        reserved_storage_bytes: row.get(15)?,
                        reserved_model_spend_cents: row.get(16)?,
                        reserved_paid_service_spend_cents: row.get(17)?,
                        write_scopes: string_vec_column(row, 18)?,
                        competition_group: row.get(19)?,
                        competition_uncertainty: row.get(20)?,
                        competition_rule: row.get(21)?,
                        legacy: row.get(22)?,
                        purpose: row.get(23)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if ready_candidates.is_empty() {
            transaction.commit()?;
            return Ok(());
        }

        let active_scopes = {
            let mut statement = transaction.prepare(
                "SELECT assignment_metadata.write_scopes_json,
                        assignment_metadata.competition_group,
                        assignment_metadata.competition_uncertainty,
                        assignment_metadata.competition_rule
                 FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
                 WHERE assignments.commission_id = ?1 AND attempts.status = ?2",
            )?;
            let rows = statement.query_map(
                params![commission_id, AttemptStatus::Running.as_str()],
                |row| {
                    Ok((
                        string_vec_column(row, 0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let (used_concurrency, used_storage, used_model_spend, used_paid_spend) = transaction
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN status = 'active' THEN concurrency_slots ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN status = 'active' THEN storage_bytes ELSE 0 END), 0),
                        COALESCE(SUM(model_spend_cents), 0),
                        COALESCE(SUM(paid_service_spend_cents), 0)
                 FROM resource_reservations
                 WHERE commission_id = ?1",
                [commission_id],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                },
            )?;
        let ceilings = Resources {
            concurrency: ready_candidates[0].max_worker_concurrency.into(),
            storage: ready_candidates[0].max_storage_bytes,
            model_spend: ready_candidates[0].max_model_spend_cents,
            paid_spend: ready_candidates[0].max_paid_service_spend_cents,
        };
        let candidates = ready_candidates
            .into_iter()
            .map(|candidate| Work {
                write_scopes: candidate.write_scopes.clone(),
                competition: competition(
                    &candidate.competition_group,
                    &candidate.competition_uncertainty,
                    &candidate.competition_rule,
                ),
                resources: Resources {
                    concurrency: candidate.reserved_concurrency_slots.into(),
                    storage: candidate.reserved_storage_bytes,
                    model_spend: candidate.reserved_model_spend_cents,
                    paid_spend: candidate.reserved_paid_service_spend_cents,
                },
                item: candidate,
            })
            .collect();
        let occupied = active_scopes
            .into_iter()
            .map(|(write_scopes, group, uncertainty, rule)| OccupiedWork {
                write_scopes,
                competition: competition(&group, &uncertainty, &rule),
            })
            .collect();
        let mut frontier = frontier::select(
            candidates,
            occupied,
            Resources {
                concurrency: used_concurrency.into(),
                storage: used_storage,
                model_spend: used_model_spend,
                paid_spend: used_paid_spend,
            },
            ceilings,
        );
        let Some(ready) = frontier.selected.drain(..).next() else {
            return Ok(());
        };
        let proposed_execution: ExecutionSpec = serde_json::from_str(&ready.execution_json)?;
        let execution = worker.assignment_execution(
            &proposed_execution,
            commission_id,
            ready.current_artifact_revision.as_deref(),
        );

        let attempt_count = transaction.query_row(
            "SELECT COUNT(*) FROM attempts
             JOIN assignments ON assignments.id = attempts.assignment_id
             WHERE assignments.commission_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM worker_configuration_failures
                   WHERE worker_configuration_failures.attempt_id = attempts.id
               )",
            [commission_id],
            |row| row.get::<_, u32>(0),
        )?;
        if attempt_count >= ready.max_attempts {
            return block_ready_assignment(
                transaction,
                commission_id,
                &ready.assignment_id,
                ready.mandate_revision,
                "max_attempts",
                "Start a new Commission with a higher max_attempts ceiling.",
            );
        }
        let now = unix_timestamp()?;
        if now.saturating_sub(ready.accepted_at) as u64 >= ready.max_elapsed_seconds {
            return block_ready_assignment(
                transaction,
                commission_id,
                &ready.assignment_id,
                ready.mandate_revision,
                "max_elapsed_seconds",
                "Start a new Commission with a higher max_elapsed_seconds ceiling.",
            );
        }
        if matches!(execution, ExecutionSpec::Deterministic)
            && ready.goal.len() as u64 > ready.max_storage_bytes
        {
            return block_ready_assignment(
                transaction,
                commission_id,
                &ready.assignment_id,
                ready.mandate_revision,
                "max_storage_bytes",
                "Start a new Commission with a max_storage_bytes ceiling large enough for the Result.",
            );
        }

        let criteria =
            load_assignment_criteria(&transaction, commission_id, &ready.logical_id, ready.legacy)?;
        let assembled_criteria = if ready.legacy {
            Vec::new()
        } else {
            load_criteria(&transaction, commission_id)?
        };
        let authority = load_authority(&transaction, commission_id)?;
        let authorized_paths = authority.paths.clone();
        let comparison_candidates = if ready.purpose == "reconciliation" {
            load_comparison_candidates(
                &transaction,
                commission_id,
                ready.competition_group.as_deref(),
            )?
        } else {
            Vec::new()
        };
        let (configuration_json, routing_rationale_json) = transaction
            .query_row(
                "SELECT selected_configuration_json, rationale_json
                 FROM assignment_routes
                 WHERE assignment_id = ?1 AND status = 'selected'",
                [&ready.assignment_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                TyrionError::InvalidRequest(format!(
                    "Assignment {} has no selected Worker Configuration",
                    ready.assignment_id
                ))
            })?;
        let configuration_value: Value = serde_json::from_str(&configuration_json)?;
        let configuration = configuration_value["id"]
            .as_str()
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "selected Worker Configuration is missing its id".into(),
                )
            })?
            .to_owned();
        let lease_ttl_seconds = worker.lease_ttl_seconds(&execution)?;
        let commission_deadline = ready
            .accepted_at
            .saturating_add(ready.max_elapsed_seconds as i64);
        let lease_expires_at = now
            .saturating_add(lease_ttl_seconds as i64)
            .min(commission_deadline);
        if lease_expires_at <= now {
            return block_ready_assignment(
                transaction,
                commission_id,
                &ready.assignment_id,
                ready.mandate_revision,
                "worker_lease",
                "Start a new Commission with enough elapsed time for an expiring Worker Lease.",
            );
        }
        let attempt_id = Uuid::new_v4().to_string();
        let lease_id = Uuid::new_v4().to_string();
        let worker_id = Uuid::new_v4().to_string();
        let worker_handle = next_worker_handle(&transaction, commission_id)?;
        let started_at_ms = unix_timestamp_millis()?;
        let skill_defaults =
            load_attempt_skill_defaults(&transaction, &ready.assignment_id, &configuration_value)?;
        let worker_context = build_worker_context_packet(
            &transaction,
            commission_id,
            &ready,
            &configuration_value,
            &criteria,
            &authority,
            &execution,
        )?;
        transaction.execute(
            "INSERT INTO attempts (
                id, assignment_id, worker_configuration, status, started_at, started_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt_id,
                ready.assignment_id,
                configuration,
                AttemptStatus::Running.as_str(),
                now,
                started_at_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO attempt_context_packets (
                attempt_id, packet_json, advisory_token_budget,
                advisory_tokens_used, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                serde_json::to_string(&worker_context.packet)?,
                worker_context.token_budget,
                worker_context.tokens_used,
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO temporary_memory_materials (
                id, commission_id, attempt_id, kind, content_json, created_at
             ) VALUES (?1, ?2, ?3, 'raw_worker_transcript', ?4, ?5)",
            params![
                format!("transcript:{attempt_id}"),
                commission_id,
                attempt_id,
                serde_json::to_string(&serde_json::json!({
                    "attempt_id": attempt_id,
                    "events": ["worker_launched"],
                }))?,
                now,
            ],
        )?;
        for (position, claim) in worker_context.claims.iter().enumerate() {
            transaction.execute(
                "INSERT INTO attempt_profile_claims (
                    attempt_id, claim_id, claim_version, position
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![attempt_id, claim.id, claim.version, position as i64],
            )?;
        }
        transaction.execute(
            "INSERT INTO workers (
                id, commission_id, assignment_id, attempt_id, handle,
                configuration_json, routing_rationale_json, status,
                latest_activity, activity_at_ms, usage_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running',
                       'Worker launched', ?8, '{}')",
            params![
                worker_id,
                commission_id,
                ready.assignment_id,
                attempt_id,
                worker_handle,
                configuration_json,
                routing_rationale_json,
                started_at_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO resource_reservations (
                attempt_id, commission_id, concurrency_slots, storage_bytes,
                model_spend_cents, paid_service_spend_cents, status, reserved_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![
                attempt_id,
                commission_id,
                ready.reserved_concurrency_slots,
                ready.reserved_storage_bytes,
                ready.reserved_model_spend_cents,
                ready.reserved_paid_service_spend_cents,
                started_at_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO worker_leases (
                id, attempt_id, issued_at, expires_at, mandate_revision, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                lease_id,
                attempt_id,
                now,
                lease_expires_at,
                ready.mandate_revision,
                WorkerLeaseStatus::Active.as_str(),
            ],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![ready.assignment_id, AssignmentStatus::Running.as_str()],
        )?;
        transaction.execute(
            "UPDATE verification_recoveries
             SET status = ?2, resolved_at = ?3
             WHERE commission_id = ?1 AND action = ?4 AND status = ?5",
            params![
                commission_id,
                VerificationRecoveryStatus::Resolved.as_str(),
                now,
                VerificationRecoveryAction::Rework.as_str(),
                VerificationRecoveryStatus::Scheduled.as_str(),
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::AttemptStarted,
            ready.mandate_revision,
            &serde_json::json!({
                "assignment_id": ready.assignment_id,
                "logical_id": ready.logical_id,
                "plan_revision": ready.plan_revision,
                "started_at_ms": started_at_ms,
                "worker_id": worker_id,
                "worker_handle": worker_handle,
                "worker_configuration": configuration_value,
            }),
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::WorkerActivity,
            ready.mandate_revision,
            &serde_json::json!({
                "worker_id": worker_id,
                "worker_handle": worker_handle,
                "assignment_id": ready.assignment_id,
                "activity": "Worker launched",
            }),
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ResourcesReserved,
            ready.mandate_revision,
            &serde_json::json!({
                "assignment_id": ready.assignment_id,
                "attempt_id": attempt_id,
                "concurrency_slots": ready.reserved_concurrency_slots,
                "storage_bytes": ready.reserved_storage_bytes,
                "model_spend_cents": ready.reserved_model_spend_cents,
                "paid_service_spend_cents": ready.reserved_paid_service_spend_cents,
                "reserved_atomically": true,
            }),
        )?;
        worker.begin_attempt(
            &attempt_id,
            ready.mandate_revision,
            ready.reserved_storage_bytes,
        )?;
        if let Err(error) = transaction.commit() {
            worker.end_attempt(&attempt_id)?;
            return Err(error.into());
        }
        let _attempt_control_scope = worker.attempt_control_scope(&attempt_id);

        let assignment = worker::AssignmentContext {
            commission_id: commission_id.to_owned(),
            assignment_id: ready.assignment_id.clone(),
            attempt_id: attempt_id.clone(),
            mandate_revision: ready.mandate_revision,
            plan_revision: ready.plan_revision,
            goal: ready.goal.clone(),
            execution,
            selected_configuration: configuration_value.clone(),
            worker_context_packet: serde_json::to_value(worker_context.packet)?,
            skill_defaults,
            criteria,
            authority,
            authorized_paths,
            declared_write_scopes: ready.write_scopes.clone(),
            comparison_candidates,
            max_storage_bytes: ready.reserved_storage_bytes,
            max_model_spend_cents: ready.reserved_model_spend_cents,
            max_paid_service_spend_cents: ready.reserved_paid_service_spend_cents,
            lease_expires_at,
        };
        let execution_result = worker.execute(&assignment);
        let execution_completed_at_ms = unix_timestamp_millis()?;
        self.connection.execute(
            "UPDATE attempts SET execution_completed_at_ms = ?2 WHERE id = ?1",
            params![attempt_id, execution_completed_at_ms],
        )?;
        let terminal_telemetry = worker.live_telemetry(&attempt_id);
        if let Some(telemetry) = terminal_telemetry.as_ref() {
            self.persist_worker_telemetry(&attempt_id, telemetry)?;
            let observed_skill_versions =
                serde_json::from_value::<Vec<SkillVersion>>(telemetry["skill_versions"].clone())?;
            if !observed_skill_versions.is_empty() {
                let transaction = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                record_observed_skill_defaults(
                    &transaction,
                    &ready.assignment_id,
                    &assignment.skill_defaults,
                    &observed_skill_versions,
                    ready.plan_revision,
                    execution_completed_at_ms / 1000,
                )?;
                transaction.commit()?;
            }
        }
        let candidate = match execution_result {
            Ok(candidate) => candidate,
            Err(error) => {
                self.connection.execute(
                    "UPDATE temporary_memory_materials
                     SET content_json = ?2
                     WHERE id = ?1 AND kind = 'raw_worker_transcript'",
                    params![
                        format!("transcript:{attempt_id}"),
                        serde_json::to_string(&serde_json::json!({
                            "attempt_id": attempt_id,
                            "adapter_events": terminal_telemetry
                                .as_ref()
                                .map_or_else(|| serde_json::json!([]), |telemetry| {
                                    telemetry["raw_adapter_events"].clone()
                                }),
                            "adapter_events_truncated": terminal_telemetry
                                .as_ref()
                                .is_some_and(|telemetry| {
                                    telemetry["raw_adapter_events_truncated"] == true
                                }),
                            "terminal_error": error.to_string(),
                        }))?,
                    ],
                )?;
                self.fail_attempt(
                    commission_id,
                    &assignment.assignment_id,
                    &attempt_id,
                    &lease_id,
                    ready.mandate_revision,
                    &error,
                )?;
                return Ok(());
            }
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE temporary_memory_materials
             SET content_json = ?2
             WHERE id = ?1 AND kind = 'raw_worker_transcript'",
            params![
                format!("transcript:{attempt_id}"),
                serde_json::to_string(&serde_json::json!({
                    "attempt_id": attempt_id,
                    "adapter_events": terminal_telemetry
                        .as_ref()
                        .map_or_else(|| serde_json::json!([]), |telemetry| {
                            telemetry["raw_adapter_events"].clone()
                        }),
                    "adapter_events_truncated": terminal_telemetry
                        .as_ref()
                        .is_some_and(|telemetry| {
                            telemetry["raw_adapter_events_truncated"] == true
                        }),
                    "worker_output": &candidate.output,
                }))?,
            ],
        )?;
        let continuation = attempt_continuation(&transaction, commission_id, &ready, &attempt_id)?;
        transaction.execute(
            "UPDATE workers
             SET status = 'succeeded', native_session_id = ?2, usage_json = ?3,
                 latest_activity = ?4, activity_at_ms = ?5
             WHERE attempt_id = ?1 AND status = 'running'",
            params![
                attempt_id,
                candidate.native_session_id,
                serde_json::to_string(&candidate.usage)?,
                candidate.latest_meaningful_activity,
                execution_completed_at_ms,
            ],
        )?;
        if matches!(continuation, AttemptContinuation::Cancelled) {
            transaction.commit()?;
            return Ok(());
        }
        let stale = matches!(continuation, AttemptContinuation::Stale { .. });
        let result_id = Uuid::new_v4().to_string();
        let result_created_at = unix_timestamp()?;
        let candidate_commits_json = serde_json::to_string(&candidate.candidate_commits)?;
        let changed_paths_json = serde_json::to_string(&candidate.changed_paths)?;
        let artifacts_json = serde_json::to_string(&candidate.artifacts)?;
        let known_effects_json = serde_json::to_string(&candidate.known_effects)?;
        transaction.execute(
            "INSERT INTO results (
                id, attempt_id, output, artifact_revision, status, created_at,
                mandate_revision, plan_revision, base_revision, candidate_commits_json,
                changed_paths_json, artifacts_json, known_effects_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                result_id,
                attempt_id,
                candidate.output,
                candidate.artifact_revision.as_str(),
                if stale {
                    ResultStatus::Superseded.as_str()
                } else {
                    ResultStatus::Candidate.as_str()
                },
                result_created_at,
                ready.mandate_revision,
                ready.plan_revision,
                candidate.base_revision,
                candidate_commits_json,
                changed_paths_json,
                artifacts_json,
                known_effects_json,
            ],
        )?;
        transaction.execute(
            "INSERT INTO temporary_memory_materials (
                id, commission_id, attempt_id, result_id, kind, content_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, 'unaccepted_artifact', ?5, ?6)",
            params![
                format!("artifact:{result_id}"),
                commission_id,
                attempt_id,
                result_id,
                serde_json::to_string(&serde_json::json!({
                    "artifact_revision": candidate.artifact_revision.as_str(),
                    "artifacts": &candidate.artifacts,
                }))?,
                result_created_at,
            ],
        )?;
        if stale {
            retain_noncurrent_result(
                &transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                false,
            )?;
            transaction.execute(
                "UPDATE workers
                 SET latest_activity = 'Stale Result retained without Integration', activity_at_ms = ?2
                 WHERE attempt_id = ?1",
                params![attempt_id, execution_completed_at_ms],
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::ResultSubmitted,
                match continuation {
                    AttemptContinuation::Stale {
                        commission_revision,
                        ..
                    } => commission_revision,
                    AttemptContinuation::Current | AttemptContinuation::Cancelled => {
                        ready.mandate_revision
                    }
                },
                &serde_json::json!({
                    "result_id": result_id,
                    "assignment_id": ready.assignment_id,
                    "revision_disposition": "stale",
                    "integrated_automatically": false,
                }),
            )?;
            transaction.commit()?;
            return Ok(());
        }
        record_superseded_profile_claims_as_edited(
            &transaction,
            &ready.assignment_id,
            &attempt_id,
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::ResultSubmitted,
            ready.mandate_revision,
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::WorkerActivity,
            ready.mandate_revision,
            &serde_json::json!({
                "attempt_id": attempt_id,
                "assignment_id": ready.assignment_id,
                "activity": candidate.latest_meaningful_activity,
                "native_session_id": candidate.native_session_id,
                "usage": candidate.usage,
            }),
        )?;
        transaction.commit()?;

        if !ready.legacy
            && candidate.changed_paths.iter().any(|path| {
                !ready
                    .write_scopes
                    .iter()
                    .any(|scope| path_is_within_scope(path, scope))
            })
        {
            self.require_reconciliation(
                commission_id,
                &ready,
                &attempt_id,
                &lease_id,
                &result_id,
                "unexpected_overlap",
                "the Result changed an authorized path outside its declared artifact scope",
                &candidate.changed_paths,
            )?;
            return Ok(());
        }

        let candidate_verification = match worker.verify_candidate(&assignment, &candidate) {
            Ok(verification) => verification,
            Err(error) => {
                self.fail_attempt(
                    commission_id,
                    &assignment.assignment_id,
                    &attempt_id,
                    &lease_id,
                    ready.mandate_revision,
                    &error,
                )?;
                return Ok(());
            }
        };
        let candidate_passed = candidate_verification
            .iter()
            .all(worker::VerificationRecord::passed);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, &ready, &attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                false,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE results SET verification_outcomes_json = ?2 WHERE id = ?1",
            params![result_id, serde_json::to_string(&candidate_verification)?],
        )?;
        record_evidence(
            &transaction,
            commission_id,
            &result_id,
            ready.mandate_revision,
            candidate.artifact_revision.as_str(),
            &candidate_verification,
        )?;
        if !candidate_passed {
            record_result_profile_claim_outcome(
                &transaction,
                &result_id,
                ProfileClaimOutcome::Rejected,
            )?;
            record_result_skill_outcomes(
                &transaction,
                commission_id,
                &ready,
                &attempt_id,
                &result_id,
                "failed",
                &candidate.usage,
            )?;
            recover_failed_verification(
                &transaction,
                commission_id,
                &ready,
                &attempt_id,
                &lease_id,
                &result_id,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.commit()?;

        if ready.competition_group.is_some() && ready.purpose != "reconciliation" {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            record_result_skill_outcomes(
                &transaction,
                commission_id,
                &ready,
                &attempt_id,
                &result_id,
                "passed",
                &candidate.usage,
            )?;
            transaction.commit()?;
            self.finish_competing_candidate(
                commission_id,
                &ready,
                &attempt_id,
                &lease_id,
                &result_id,
            )?;
            return Ok(());
        }

        worker.wait_before_integration();
        let integration_lock = worker.commission_integration_lock(commission_id)?;
        let integration_guard = integration_lock.lock().map_err(|_| {
            TyrionError::InvalidRequest("Commission Integration lock is unavailable".into())
        })?;
        let integration_transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation =
            attempt_continuation(&integration_transaction, commission_id, &ready, &attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &integration_transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                false,
            )?;
            integration_transaction.commit()?;
            return Ok(());
        }
        integration_transaction.commit()?;
        let integrated = match worker.integrate(&assignment, &candidate) {
            Ok(integrated) => integrated,
            Err(TyrionError::IntegrationFailure { kind, message }) if !ready.legacy => {
                drop(integration_guard);
                self.require_reconciliation(
                    commission_id,
                    &ready,
                    &attempt_id,
                    &lease_id,
                    &result_id,
                    kind.as_str(),
                    &message,
                    &candidate.changed_paths,
                )?;
                return Ok(());
            }
            Err(error) => {
                drop(integration_guard);
                self.fail_attempt(
                    commission_id,
                    &assignment.assignment_id,
                    &attempt_id,
                    &lease_id,
                    ready.mandate_revision,
                    &error,
                )?;
                return Ok(());
            }
        };
        worker.wait_after_external_integration();
        let integration_transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation =
            attempt_continuation(&integration_transaction, commission_id, &ready, &attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            worker.rollback_integration(&integrated)?;
            retain_noncurrent_result(
                &integration_transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                false,
            )?;
            integration_transaction.commit()?;
            drop(integration_guard);
            return Ok(());
        }
        integration_transaction.execute(
            "UPDATE results SET integrated_artifact_revision = ?2 WHERE id = ?1",
            params![result_id, integrated.artifact_revision.as_str()],
        )?;
        integration_transaction.execute(
            "UPDATE commissions SET artifact_revision = ?2 WHERE id = ?1",
            params![commission_id, integrated.artifact_revision.as_str()],
        )?;
        integration_transaction.commit()?;
        drop(integration_guard);
        worker.wait_after_integration();
        let mut artifacts = candidate.artifacts.clone();
        artifacts.extend(integrated.artifacts.clone());
        if !ready.legacy {
            return self.finish_planned_assignment(
                commission_id,
                worker,
                &ready,
                &assignment,
                &attempt_id,
                &lease_id,
                &result_id,
                candidate_verification,
                assembled_criteria,
                artifacts,
                integrated,
                &candidate.usage,
            );
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, &ready, &attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                true,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE results
             SET artifacts_json = ?2, integrated_artifact_revision = ?3
             WHERE id = ?1",
            params![
                result_id,
                serde_json::to_string(&artifacts)?,
                integrated.artifact_revision.as_str(),
            ],
        )?;
        transaction.execute(
            "UPDATE commissions SET artifact_revision = ?2 WHERE id = ?1",
            params![commission_id, integrated.artifact_revision.as_str()],
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::ResultIntegrated,
            ready.mandate_revision,
        )?;
        transaction.commit()?;

        let integrated_verification = match worker.verify_integrated(&assignment, &integrated) {
            Ok(verification) => verification,
            Err(error) => {
                self.fail_attempt(
                    commission_id,
                    &assignment.assignment_id,
                    &attempt_id,
                    &lease_id,
                    ready.mandate_revision,
                    &error,
                )?;
                return Ok(());
            }
        };
        let integrated_passed = integrated_verification
            .iter()
            .all(worker::VerificationRecord::passed);
        let mut all_verification = candidate_verification;
        all_verification.extend(integrated_verification.clone());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, &ready, &attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                &attempt_id,
                &lease_id,
                &result_id,
                true,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE results SET verification_outcomes_json = ?2 WHERE id = ?1",
            params![result_id, serde_json::to_string(&all_verification)?],
        )?;
        record_evidence(
            &transaction,
            commission_id,
            &result_id,
            ready.mandate_revision,
            integrated.artifact_revision.as_str(),
            &integrated_verification,
        )?;
        refresh_criterion_statuses(
            &transaction,
            commission_id,
            ready.mandate_revision,
            integrated.artifact_revision.as_str(),
        )?;
        if !integrated_passed {
            record_result_profile_claim_outcome(
                &transaction,
                &result_id,
                ProfileClaimOutcome::Rejected,
            )?;
            record_result_skill_outcomes(
                &transaction,
                commission_id,
                &ready,
                &attempt_id,
                &result_id,
                "failed",
                &candidate.usage,
            )?;
            finish_verification(
                &transaction,
                &assignment.assignment_id,
                &attempt_id,
                &lease_id,
                AssignmentStatus::VerificationFailed,
            )?;
            transaction.commit()?;
            return Ok(());
        }

        record_result_skill_outcomes(
            &transaction,
            commission_id,
            &ready,
            &attempt_id,
            &result_id,
            "passed",
            &candidate.usage,
        )?;

        let every_criterion_passed = transaction.query_row(
            "SELECT NOT EXISTS(
                SELECT 1 FROM criteria WHERE commission_id = ?1 AND status != ?2
             )",
            params![commission_id, CriterionStatus::Passed.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if !every_criterion_passed {
            finish_verification(
                &transaction,
                &assignment.assignment_id,
                &attempt_id,
                &lease_id,
                AssignmentStatus::VerificationPending,
            )?;
            transaction.commit()?;
            return Ok(());
        }

        complete_commission(
            &transaction,
            CompletionTransition {
                commission_id,
                result_id: &result_id,
                assignment_id: &assignment.assignment_id,
                attempt_id: Some(&attempt_id),
                lease_id: Some(&lease_id),
                mandate_revision: ready.mandate_revision,
                artifact_revision: integrated.artifact_revision.as_str(),
                goal: &ready.goal,
            },
        )?;

        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_planned_assignment(
        &mut self,
        commission_id: &str,
        worker: &worker::WorkerRuntime,
        ready: &ReadyAssignmentDispatch,
        assignment: &worker::AssignmentContext,
        attempt_id: &str,
        lease_id: &str,
        result_id: &str,
        candidate_verification: Vec<worker::VerificationRecord>,
        assembled_criteria: Vec<worker::CriterionDefinition>,
        artifacts: Vec<worker::ArtifactRecord>,
        integrated: worker::IntegratedResult,
        usage: &Value,
    ) -> Result<(), TyrionError> {
        let mut assembled_assignment = assignment.clone();
        assembled_assignment.criteria = assembled_criteria;
        let integrated_verification =
            match worker.verify_integrated(&assembled_assignment, &integrated) {
                Ok(verification) => verification,
                Err(error) => {
                    let transaction = self
                        .connection
                        .transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let continuation =
                        attempt_continuation(&transaction, commission_id, ready, attempt_id)?;
                    if !matches!(continuation, AttemptContinuation::Current) {
                        retain_noncurrent_result(
                            &transaction,
                            continuation,
                            attempt_id,
                            lease_id,
                            result_id,
                            true,
                        )?;
                        transaction.commit()?;
                        return Ok(());
                    }
                    worker.rollback_integration(&integrated)?;
                    transaction.execute(
                        "UPDATE results SET integrated_artifact_revision = NULL WHERE id = ?1",
                        [result_id],
                    )?;
                    transaction.execute(
                        "UPDATE commissions SET artifact_revision = ?2 WHERE id = ?1",
                        params![commission_id, ready.current_artifact_revision.as_deref()],
                    )?;
                    transaction.commit()?;
                    self.require_reconciliation(
                        commission_id,
                        ready,
                        attempt_id,
                        lease_id,
                        result_id,
                        "integrated_verification_unavailable",
                        &error.to_string(),
                        &ready.write_scopes,
                    )?;
                    return Ok(());
                }
            };

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, ready, attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                attempt_id,
                lease_id,
                result_id,
                true,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        let expected_criteria = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT planned_assignment_criteria.criterion_id
                 FROM planned_assignment_criteria
                 LEFT JOIN assignment_metadata
                   ON assignment_metadata.logical_id = planned_assignment_criteria.assignment_logical_id
                 LEFT JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                                      AND assignments.commission_id = planned_assignment_criteria.commission_id
                 WHERE planned_assignment_criteria.commission_id = ?1
                   AND (planned_assignment_criteria.assignment_logical_id = ?2
                        OR assignments.status = ?3)",
            )?;
            let rows = statement.query_map(
                params![
                    commission_id,
                    ready.logical_id,
                    AssignmentStatus::Accepted.as_str()
                ],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<Result<HashSet<_>, _>>()?
        };
        let integrated_regression = expected_criteria.iter().any(|criterion_id| {
            let records = integrated_verification
                .iter()
                .filter(|record| &record.criterion_id == criterion_id)
                .collect::<Vec<_>>();
            records.is_empty() || records.iter().any(|record| !record.passed())
        });
        if integrated_regression {
            let mut retained_verification = candidate_verification.clone();
            retained_verification.extend(integrated_verification.clone());
            transaction.execute(
                "UPDATE results SET verification_outcomes_json = ?2 WHERE id = ?1",
                params![result_id, serde_json::to_string(&retained_verification)?],
            )?;
            record_evidence(
                &transaction,
                commission_id,
                result_id,
                ready.mandate_revision,
                integrated.artifact_revision.as_str(),
                &integrated_verification,
            )?;
            worker.rollback_integration(&integrated)?;
            transaction.execute(
                "UPDATE results SET integrated_artifact_revision = NULL WHERE id = ?1",
                [result_id],
            )?;
            transaction.execute(
                "UPDATE commissions SET artifact_revision = ?2 WHERE id = ?1",
                params![commission_id, ready.current_artifact_revision.as_deref()],
            )?;
            record_result_profile_claim_outcome(
                &transaction,
                result_id,
                ProfileClaimOutcome::Rejected,
            )?;
            transaction.commit()?;
            self.require_reconciliation(
                commission_id,
                ready,
                attempt_id,
                lease_id,
                result_id,
                "integrated_regression",
                "fresh assembled-state verification regressed a criterion owned by integrated work",
                &ready.write_scopes,
            )?;
            return Ok(());
        }

        let mut all_verification = candidate_verification;
        all_verification.extend(integrated_verification.clone());
        transaction.execute(
            "UPDATE results
             SET artifacts_json = ?2, integrated_artifact_revision = ?3,
                 verification_outcomes_json = ?4
             WHERE id = ?1",
            params![
                result_id,
                serde_json::to_string(&artifacts)?,
                integrated.artifact_revision.as_str(),
                serde_json::to_string(&all_verification)?,
            ],
        )?;
        transaction.execute(
            "UPDATE commissions SET artifact_revision = ?2 WHERE id = ?1",
            params![commission_id, integrated.artifact_revision.as_str()],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ResultIntegrated,
            ready.mandate_revision,
            &serde_json::json!({
                "assignment_id": ready.assignment_id,
                "logical_id": ready.logical_id,
                "result_id": result_id,
                "plan_revision": ready.plan_revision,
                "artifact_revision": integrated.artifact_revision.as_str(),
                "serialized": true,
            }),
        )?;
        record_evidence(
            &transaction,
            commission_id,
            result_id,
            ready.mandate_revision,
            integrated.artifact_revision.as_str(),
            &integrated_verification,
        )?;
        refresh_criterion_statuses(
            &transaction,
            commission_id,
            ready.mandate_revision,
            integrated.artifact_revision.as_str(),
        )?;
        record_result_skill_outcomes(
            &transaction,
            commission_id,
            ready,
            attempt_id,
            result_id,
            "passed",
            usage,
        )?;
        accept_planned_result(
            &transaction,
            commission_id,
            PlannedAcceptance {
                assignment_id: &ready.assignment_id,
                attempt_id,
                lease_id,
                result_id,
                mandate_revision: ready.mandate_revision,
            },
        )?;
        if ready.purpose == "reconciliation" {
            if let Some(group) = ready.competition_group.as_deref() {
                supersede_competing_candidates(
                    &transaction,
                    commission_id,
                    group,
                    &ready.assignment_id,
                    result_id,
                )?;
            }
        }
        record_useful_concurrency(
            &transaction,
            commission_id,
            attempt_id,
            ready.mandate_revision,
        )?;
        advance_plan_after_evidence(
            &transaction,
            commission_id,
            ready.mandate_revision,
            result_id,
        )?;

        let every_assignment_accepted = transaction.query_row(
            "SELECT NOT EXISTS(
                SELECT 1 FROM planned_assignments
                WHERE planned_assignments.commission_id = ?1
                  AND planned_assignments.purpose != 'reconciliation'
                  AND NOT EXISTS(
                      SELECT 1 FROM assignment_metadata
                      JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                      WHERE assignments.commission_id = planned_assignments.commission_id
                        AND assignment_metadata.logical_id = planned_assignments.logical_id
                        AND assignments.status IN (?2, ?3)
                  )
             )",
            params![
                commission_id,
                AssignmentStatus::Accepted.as_str(),
                AssignmentStatus::Superseded.as_str()
            ],
            |row| row.get::<_, bool>(0),
        )?;
        let every_criterion_passed = transaction.query_row(
            "SELECT NOT EXISTS(
                SELECT 1 FROM criteria WHERE commission_id = ?1 AND status != ?2
             )",
            params![commission_id, CriterionStatus::Passed.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if every_assignment_accepted && every_criterion_passed {
            complete_planned_commission(
                &transaction,
                commission_id,
                ready.mandate_revision,
                integrated.artifact_revision.as_str(),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn finish_competing_candidate(
        &mut self,
        commission_id: &str,
        ready: &ReadyAssignmentDispatch,
        attempt_id: &str,
        lease_id: &str,
        result_id: &str,
    ) -> Result<(), TyrionError> {
        let group = ready
            .competition_group
            .as_deref()
            .expect("competing candidate has a group");
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, ready, attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                attempt_id,
                lease_id,
                result_id,
                false,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        finish_verification(
            &transaction,
            &ready.assignment_id,
            attempt_id,
            lease_id,
            AssignmentStatus::VerificationPending,
        )?;
        transaction.execute(
            "UPDATE attempts SET revision_disposition = 'requires_revalidation' WHERE id = ?1",
            [attempt_id],
        )?;
        transaction.execute(
            "UPDATE results SET revision_disposition = 'requires_revalidation' WHERE id = ?1",
            [result_id],
        )?;
        let (member_count, completed_count) = transaction.query_row(
            "SELECT COUNT(*),
                    SUM(CASE WHEN assignments.status = ?3 THEN 1 ELSE 0 END)
             FROM assignments
             JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
             WHERE assignments.commission_id = ?1
               AND assignment_metadata.competition_group = ?2
               AND assignment_metadata.purpose != 'reconciliation'",
            params![
                commission_id,
                group,
                AssignmentStatus::VerificationPending.as_str()
            ],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        if member_count < 2 || member_count != completed_count {
            transaction.commit()?;
            return Ok(());
        }

        let already_planned = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM planned_assignments
                WHERE commission_id = ?1 AND purpose = 'reconciliation'
                  AND competition_group = ?2
             )",
            params![commission_id, group],
            |row| row.get::<_, bool>(0),
        )?;
        if already_planned {
            transaction.commit()?;
            return Ok(());
        }

        let plan_revision = transaction.query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM commission_plans
             WHERE commission_id = ?1",
            [commission_id],
            |row| row.get::<_, i64>(0),
        )?;
        let logical_id = format!("compare-{}", Uuid::new_v4());
        let uncertainty = ready
            .competition_uncertainty
            .as_deref()
            .expect("competing candidate has an uncertainty");
        let comparison_rule = ready
            .competition_rule
            .as_deref()
            .expect("competing candidate has a comparison rule");
        let contenders = {
            let mut statement = transaction.prepare(
                "SELECT results.id, results.artifact_revision, results.output,
                        results.changed_paths_json
                 FROM results
                 JOIN attempts ON attempts.id = results.attempt_id
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
                 WHERE assignments.commission_id = ?1
                   AND assignment_metadata.competition_group = ?2
                   AND assignment_metadata.purpose != 'reconciliation'
                   AND results.status = ?3
                 ORDER BY assignment_metadata.position, results.id",
            )?;
            let rows = statement.query_map(
                params![commission_id, group, ResultStatus::Candidate.as_str()],
                |row| {
                    Ok(serde_json::json!({
                        "result_id": row.get::<_, String>(0)?,
                        "artifact_revision": row.get::<_, String>(1)?,
                        "summary": row.get::<_, String>(2)?,
                        "changed_paths": serde_json::from_str::<Value>(&row.get::<_, String>(3)?)
                            .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            ))?,
                    }))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let write_scopes = {
            let mut scopes = Vec::new();
            let mut statement = transaction.prepare(
                "SELECT write_scopes_json FROM assignment_metadata
                 JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                 WHERE assignments.commission_id = ?1
                   AND assignment_metadata.competition_group = ?2
                   AND assignment_metadata.purpose != 'reconciliation'",
            )?;
            let rows = statement.query_map(params![commission_id, group], |row| {
                string_vec_column(row, 0)
            })?;
            for member_scopes in rows {
                for scope in member_scopes? {
                    if !scopes.contains(&scope) {
                        scopes.push(scope);
                    }
                }
            }
            scopes
        };
        let criteria_ids = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT planned_assignment_criteria.criterion_id
                 FROM planned_assignment_criteria
                 JOIN planned_assignments
                   ON planned_assignments.commission_id = planned_assignment_criteria.commission_id
                  AND planned_assignments.logical_id = planned_assignment_criteria.assignment_logical_id
                 WHERE planned_assignments.commission_id = ?1
                   AND planned_assignments.competition_group = ?2
                 ORDER BY planned_assignment_criteria.position,
                          planned_assignment_criteria.criterion_id",
            )?;
            let rows = statement
                .query_map(params![commission_id, group], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let comparison_budget = {
            let mut statement = transaction.prepare(
                "SELECT assignment_metadata.concurrency_slots,
                        assignment_metadata.max_storage_bytes,
                        assignment_metadata.max_model_spend_cents,
                        assignment_metadata.max_paid_service_spend_cents
                 FROM assignment_metadata
                 JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                 WHERE assignments.commission_id = ?1
                   AND assignment_metadata.competition_group = ?2
                   AND assignment_metadata.purpose != 'reconciliation'",
            )?;
            let rows = statement.query_map(params![commission_id, group], |row| {
                Ok(Resources {
                    concurrency: row.get(0)?,
                    storage: row.get(1)?,
                    model_spend: row.get(2)?,
                    paid_spend: row.get(3)?,
                })
            })?;
            comparison_resources(rows.collect::<Result<Vec<_>, _>>()?)?
        };
        let goal = format!(
            "Reconcile competing Results for uncertainty: {uncertainty}. Apply comparison rule: {comparison_rule}. Candidate Evidence: {}. Authorized reconciliation write scope: {}",
            serde_json::to_string(&contenders)?,
            write_scopes.first().map(String::as_str).unwrap_or("")
        );
        transaction.execute(
            "INSERT INTO planned_assignments (
                commission_id, logical_id, position, goal, purpose,
                read_scopes_json, write_scopes_json, concurrency_slots,
                max_storage_bytes, max_model_spend_cents,
                max_paid_service_spend_cents, competition_group,
                competition_uncertainty, competition_rule, created_plan_revision
             ) VALUES (
                ?1, ?2,
                (SELECT COALESCE(MAX(position), -1) + 1 FROM planned_assignments WHERE commission_id = ?1),
                ?3, 'reconciliation', ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
             )",
            params![
                commission_id,
                logical_id,
                goal,
                serde_json::to_string(&write_scopes)?,
                comparison_budget.concurrency,
                comparison_budget.storage,
                comparison_budget.model_spend,
                comparison_budget.paid_spend,
                group,
                uncertainty,
                comparison_rule,
                plan_revision,
            ],
        )?;
        for (position, criterion_id) in criteria_ids.iter().enumerate() {
            transaction.execute(
                "INSERT INTO planned_assignment_criteria (
                    commission_id, assignment_logical_id, criterion_id, position
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![commission_id, logical_id, criterion_id, position as i64],
            )?;
        }
        let now = unix_timestamp()?;
        let reconciliation_id = insert_ready_assignment(
            &transaction,
            commission_id,
            &logical_id,
            plan_revision,
            ready.mandate_revision,
            false,
            now,
        )?;
        let execution_frontier = execution_frontier_logical_ids(&transaction, commission_id)?;
        let snapshot = serde_json::json!({
            "reason": "competition_comparison",
            "uncertainty": uncertainty,
            "comparison_rule": comparison_rule,
            "candidate_results": contenders,
            "reconciliation_assignment_id": reconciliation_id,
            "execution_frontier": execution_frontier,
        });
        transaction.execute(
            "INSERT INTO commission_plans (
                commission_id, revision, source, reason, snapshot_json, created_at
             ) VALUES (?1, ?2, 'control_plane', ?3, ?4, ?5)",
            params![
                commission_id,
                plan_revision,
                goal,
                serde_json::to_string(&snapshot)?,
                now
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::PlanRevised,
            ready.mandate_revision,
            &serde_json::json!({
                "plan_revision": plan_revision,
                "reason": "competition_comparison",
                "source_result_id": result_id,
                "execution_frontier": execution_frontier,
            }),
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ReconciliationRequired,
            ready.mandate_revision,
            &serde_json::json!({
                "kind": "competition_comparison",
                "uncertainty": uncertainty,
                "comparison_rule": comparison_rule,
                "candidate_results": contenders,
                "reconciliation_assignment_id": reconciliation_id,
                "silent_winner_selected": false,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn require_reconciliation(
        &mut self,
        commission_id: &str,
        ready: &ReadyAssignmentDispatch,
        attempt_id: &str,
        lease_id: &str,
        result_id: &str,
        kind: &str,
        message: &str,
        affected_paths: &[String],
    ) -> Result<(), TyrionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = attempt_continuation(&transaction, commission_id, ready, attempt_id)?;
        if !matches!(continuation, AttemptContinuation::Current) {
            retain_noncurrent_result(
                &transaction,
                continuation,
                attempt_id,
                lease_id,
                result_id,
                false,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        let now = unix_timestamp()?;
        release_successful_attempt(
            &transaction,
            SuccessfulAttemptRelease {
                attempt_id,
                lease_id,
            },
        )?;
        transaction.execute(
            "UPDATE attempts SET revision_disposition = 'requires_revalidation' WHERE id = ?1",
            [attempt_id],
        )?;
        transaction.execute(
            "UPDATE results SET revision_disposition = 'requires_revalidation' WHERE id = ?1",
            [result_id],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![
                ready.assignment_id,
                AssignmentStatus::VerificationFailed.as_str()
            ],
        )?;

        let plan_revision = transaction.query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM commission_plans
             WHERE commission_id = ?1",
            [commission_id],
            |row| row.get::<_, i64>(0),
        )?;
        let logical_id = format!("reconcile-{}", Uuid::new_v4());
        let competition_group = format!("reconciliation-{}", ready.assignment_id);
        let comparison_rule =
            "produce a fresh Result that passes candidate and assembled verification";
        let reconciliation_budget = Resources {
            concurrency: ready.reserved_concurrency_slots.into(),
            storage: ready.max_storage_bytes,
            model_spend: ready.reserved_model_spend_cents,
            paid_spend: ready.reserved_paid_service_spend_cents,
        };
        let goal = format!(
            "Reconcile {kind} for Assignment {} without selecting a silent winner: {message}. Authorized reconciliation write scope: {}",
            ready.logical_id,
            affected_paths.first().map(String::as_str).unwrap_or("")
        );
        transaction.execute(
            "UPDATE assignment_metadata
             SET competition_group = ?2, competition_uncertainty = ?3,
                 competition_rule = ?4
             WHERE assignment_id = ?1",
            params![
                ready.assignment_id,
                competition_group,
                message,
                comparison_rule
            ],
        )?;
        transaction.execute(
            "INSERT INTO planned_assignments (
                commission_id, logical_id, position, goal, purpose,
                read_scopes_json, write_scopes_json, concurrency_slots,
                max_storage_bytes, max_model_spend_cents,
                max_paid_service_spend_cents, competition_group,
                competition_uncertainty, competition_rule, created_plan_revision
             ) VALUES (
                ?1, ?2,
                (SELECT COALESCE(MAX(position), -1) + 1 FROM planned_assignments WHERE commission_id = ?1),
                ?3, 'reconciliation', ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
             )",
            params![
                commission_id,
                logical_id,
                goal,
                serde_json::to_string(affected_paths)?,
                reconciliation_budget.concurrency,
                reconciliation_budget.storage,
                reconciliation_budget.model_spend,
                reconciliation_budget.paid_spend,
                competition_group,
                message,
                comparison_rule,
                plan_revision,
            ],
        )?;
        transaction.execute(
            "INSERT INTO planned_assignment_criteria (
                commission_id, assignment_logical_id, criterion_id, position
             )
             SELECT commission_id, ?3, criterion_id, position
             FROM planned_assignment_criteria
             WHERE commission_id = ?1 AND assignment_logical_id = ?2",
            params![commission_id, ready.logical_id, logical_id],
        )?;
        let reconciliation_id = insert_ready_assignment(
            &transaction,
            commission_id,
            &logical_id,
            plan_revision,
            ready.mandate_revision,
            false,
            now,
        )?;
        let execution_frontier = execution_frontier_logical_ids(&transaction, commission_id)?;
        let snapshot = serde_json::json!({
            "reason": kind,
            "source_result_id": result_id,
            "reconciliation_assignment_id": reconciliation_id,
            "affected_paths": affected_paths,
            "execution_frontier": execution_frontier,
        });
        transaction.execute(
            "INSERT INTO commission_plans (
                commission_id, revision, source, reason, snapshot_json, created_at
             ) VALUES (?1, ?2, 'control_plane', ?3, ?4, ?5)",
            params![
                commission_id,
                plan_revision,
                goal,
                serde_json::to_string(&snapshot)?,
                now
            ],
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::PlanRevised,
            ready.mandate_revision,
            &serde_json::json!({
                "plan_revision": plan_revision,
                "reason": kind,
                "source_result_id": result_id,
                "execution_frontier": execution_frontier,
            }),
        )?;
        record_event_with_payload(
            &transaction,
            commission_id,
            EventKind::ReconciliationRequired,
            ready.mandate_revision,
            &serde_json::json!({
                "kind": kind,
                "message": message,
                "source_assignment_id": ready.assignment_id,
                "source_result_id": result_id,
                "reconciliation_assignment_id": reconciliation_id,
                "affected_paths": affected_paths,
                "silent_winner_selected": false,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn persist_worker_telemetry(
        &self,
        attempt_id: &str,
        telemetry: &Value,
    ) -> Result<(), TyrionError> {
        let native_session_id = telemetry["native_session_id"].as_str();
        let usage = &telemetry["usage"];
        let latest_activity = telemetry["latest_meaningful_activity"].as_str();
        let activity_at_ms = telemetry["activity_at_ms"].as_i64();
        self.connection.execute(
            "UPDATE workers
             SET native_session_id = COALESCE(?2, native_session_id),
                 usage_json = CASE WHEN ?3 = '{}' THEN usage_json ELSE ?3 END,
                 latest_activity = COALESCE(?4, latest_activity),
                 activity_at_ms = COALESCE(?5, activity_at_ms)
             WHERE attempt_id = ?1",
            params![
                attempt_id,
                native_session_id,
                serde_json::to_string(usage)?,
                latest_activity,
                activity_at_ms,
            ],
        )?;
        Ok(())
    }

    fn fail_attempt(
        &mut self,
        commission_id: &str,
        assignment_id: &str,
        attempt_id: &str,
        lease_id: &str,
        mandate_revision: i64,
        error: &TyrionError,
    ) -> Result<(), TyrionError> {
        let transaction = self.connection.transaction()?;
        let current_attempt_status = transaction.query_row(
            "SELECT status FROM attempts WHERE id = ?1",
            [attempt_id],
            |row| row.get::<_, String>(0),
        )?;
        if current_attempt_status != AttemptStatus::Running.as_str() {
            transaction.commit()?;
            return Ok(());
        }
        let now = unix_timestamp()?;
        let now_ms = unix_timestamp_millis()?;
        let interrupted = matches!(error, TyrionError::WorkerInterrupted);
        let timed_out = matches!(error, TyrionError::WatchdogContained { .. });
        let acknowledged_integrated = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM results
                JOIN attempts ON attempts.id = results.attempt_id
                JOIN assignments ON assignments.id = attempts.assignment_id
                JOIN commissions ON commissions.id = assignments.commission_id
                WHERE results.attempt_id = ?1 AND commissions.id = ?2
                  AND commissions.artifact_revision IS NOT NULL
                  AND results.integrated_artifact_revision = commissions.artifact_revision
                  AND results.status != 'accepted'
             )",
            params![attempt_id, commission_id],
            |row| row.get::<_, bool>(0),
        )?;
        let attempt_status = if interrupted {
            AttemptStatus::Interrupted
        } else if timed_out {
            AttemptStatus::TimedOut
        } else {
            AttemptStatus::Failed
        };
        transaction.execute(
            "UPDATE attempts
                 SET status = ?2, completed_at = ?3,
                     execution_completed_at_ms = COALESCE(execution_completed_at_ms, ?4),
                     completed_at_ms = ?4, revision_disposition = ?5
                 WHERE id = ?1",
            params![
                attempt_id,
                attempt_status.as_str(),
                now,
                now_ms,
                if acknowledged_integrated {
                    "requires_revalidation"
                } else {
                    "retained"
                }
            ],
        )?;
        record_attempt_profile_claim_outcome(
            &transaction,
            attempt_id,
            ProfileClaimOutcome::Rejected,
        )?;
        if acknowledged_integrated {
            transaction.execute(
                "UPDATE results
                 SET status = 'candidate', revision_disposition = 'requires_revalidation'
                 WHERE attempt_id = ?1 AND status != 'accepted'
                   AND integrated_artifact_revision = (
                       SELECT artifact_revision FROM commissions WHERE id = ?2
                   )",
                params![attempt_id, commission_id],
            )?;
            transaction.execute(
                "UPDATE results
                 SET status = 'superseded', revision_disposition = 'retained'
                 WHERE attempt_id = ?1 AND status != 'accepted'
                   AND integrated_artifact_revision IS NOT (
                       SELECT artifact_revision FROM commissions WHERE id = ?2
                   )",
                params![attempt_id, commission_id],
            )?;
        } else {
            transaction.execute(
                "UPDATE results
                 SET status = 'superseded', revision_disposition = 'retained'
                 WHERE attempt_id = ?1 AND status != 'accepted'",
                [attempt_id],
            )?;
        }
        let attempt_count = transaction.query_row(
            "SELECT COUNT(*) FROM attempts
             JOIN assignments ON assignments.id = attempts.assignment_id
             WHERE assignments.commission_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM worker_configuration_failures
                   WHERE worker_configuration_failures.attempt_id = attempts.id
               )",
            [commission_id],
            |row| row.get::<_, u32>(0),
        )?;
        let max_attempts = transaction.query_row(
            "SELECT max_attempts FROM resource_ceilings WHERE commission_id = ?1",
            [commission_id],
            |row| row.get::<_, u32>(0),
        )?;
        let transient_equivalence_key = match error {
            TyrionError::WorkerLeaseExpired { .. } => Some("worker_timed_out"),
            TyrionError::WatchdogContained { signal } => Some(*signal),
            _ => None,
        };
        let prior_equivalent_failures = transient_equivalence_key
            .map(|equivalence_key| {
                transaction.query_row(
                    "SELECT COUNT(*) FROM attempt_recoveries
                     WHERE assignment_id = ?1 AND equivalence_key = ?2",
                    params![assignment_id, equivalence_key],
                    |row| row.get::<_, u32>(0),
                )
            })
            .transpose()?
            .unwrap_or(0);
        let retry_available = attempt_count < max_attempts && prior_equivalent_failures == 0;
        let replan_required = attempt_count < max_attempts && prior_equivalent_failures > 0;
        let (
            lease_status,
            assignment_status,
            blocker_code,
            requirement,
            classification,
            equivalence_key,
            action,
            unavailable,
        ) = match error {
            _ if acknowledged_integrated => (
                if matches!(error, TyrionError::WorkerLeaseExpired { .. }) {
                    WorkerLeaseStatus::Expired
                } else {
                    WorkerLeaseStatus::Revoked
                },
                AssignmentStatus::VerificationFailed,
                "integrated_revalidation".to_owned(),
                "Revalidate the retained integrated Result against the current mandate before any further execution."
                    .to_owned(),
                "repairable_context",
                "integrated_revalidation",
                "block",
                false,
            ),
            TyrionError::WorkerLeaseExpired { .. } => (
                WorkerLeaseStatus::Expired,
                if retry_available {
                    AssignmentStatus::Ready
                } else if replan_required {
                    AssignmentStatus::AttentionRequired
                } else {
                    AssignmentStatus::ResourceBlocked
                },
                "worker_timed_out".to_owned(),
                if retry_available {
                    format!(
                        "{error}. Retry the same Worker Configuration once after confirming the transient lease failure is contained."
                    )
                } else if replan_required {
                    format!(
                        "{error}. Revise the Assignment decomposition or provide a different eligible Worker Configuration after the second equivalent timeout."
                    )
                } else {
                    format!(
                        "{error}. Start a linked Commission with enough max_attempts and elapsed time to continue the timed-out Assignment."
                    )
                },
                "transient",
                "worker_timed_out",
                if retry_available {
                    "retry"
                } else if replan_required {
                    "replan"
                } else {
                    "block"
                },
                false,
            ),
            TyrionError::WatchdogContained { signal } => (
                WorkerLeaseStatus::Revoked,
                if retry_available {
                    AssignmentStatus::Ready
                } else if replan_required {
                    AssignmentStatus::AttentionRequired
                } else {
                    AssignmentStatus::ResourceBlocked
                },
                format!("watchdog_{signal}"),
                if retry_available {
                    format!(
                        "Retry once after correcting the Watchdog {signal} condition and confirming containment."
                    )
                } else if replan_required {
                    format!(
                        "Revise the Assignment decomposition or provide a different eligible Worker Configuration after the second equivalent Watchdog {signal} finding."
                    )
                } else {
                    format!(
                        "Resolve the Watchdog {signal} condition and provide a linked Commission with another Attempt."
                    )
                },
                "transient",
                *signal,
                if retry_available {
                    "retry"
                } else if replan_required {
                    "replan"
                } else {
                    "block"
                },
                false,
            ),
            TyrionError::StorageCeilingExceeded {
                required_bytes,
                ceiling_bytes,
            } => (
                WorkerLeaseStatus::Revoked,
                AssignmentStatus::ResourceBlocked,
                "max_storage_bytes".to_owned(),
                format!(
                    "Git artifacts require at least {required_bytes} bytes; start a new Commission with max_storage_bytes of {required_bytes} or more (current ceiling: {ceiling_bytes})."
                ),
                "resource",
                "max_storage_bytes",
                "block",
                false,
            ),
            TyrionError::WorkerInterrupted => (
                WorkerLeaseStatus::Revoked,
                AssignmentStatus::AttentionRequired,
                "worker_interrupted".to_owned(),
                "Review the interrupted Assignment and explicitly retry, reroute, revise, or cancel it."
                    .to_owned(),
                "interrupted",
                "principal_interruption",
                "await_principal",
                false,
            ),
            TyrionError::WorkerConfigurationUnavailable { .. }
            | TyrionError::RequiredSkillUnavailable { .. } => (
                WorkerLeaseStatus::Revoked,
                AssignmentStatus::Ready,
                "worker_configuration_unavailable".to_owned(),
                error.to_string(),
                "poor_fit",
                "worker_configuration_unavailable",
                "reroute",
                true,
            ),
            _ => (
                WorkerLeaseStatus::Revoked,
                AssignmentStatus::VerificationFailed,
                "worker_execution_failed".to_owned(),
                error.to_string(),
                "authority",
                "worker_execution_failed",
                "block",
                false,
            ),
        };
        transaction.execute(
            "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
            params![lease_id, lease_status.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE resource_reservations
             SET status = 'revoked', released_at = ?2 WHERE attempt_id = ?1",
            params![attempt_id, now_ms],
        )?;
        if unavailable {
            transaction.execute(
                "UPDATE resource_reservations
                 SET model_spend_cents = 0, paid_service_spend_cents = 0
                 WHERE attempt_id = ?1",
                [attempt_id],
            )?;
        }
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![assignment_id, assignment_status.as_str()],
        )?;
        transaction.execute(
            "UPDATE workers
             SET status = ?2, latest_activity = ?3, activity_at_ms = ?4
             WHERE attempt_id = ?1",
            params![
                attempt_id,
                attempt_status.as_str(),
                if interrupted {
                    "Worker interrupted"
                } else if timed_out {
                    "Worker contained after timeout"
                } else {
                    "Worker failed"
                },
                now_ms,
            ],
        )?;
        let unavailable_configuration_id = match error {
            TyrionError::WorkerConfigurationUnavailable {
                configuration_id, ..
            }
            | TyrionError::RequiredSkillUnavailable {
                configuration_id, ..
            } => Some(configuration_id),
            _ => None,
        };
        if let Some(configuration_id) = unavailable_configuration_id {
            transaction.execute(
                "INSERT OR REPLACE INTO worker_configuration_failures (
                    attempt_id, assignment_id, configuration_id, reason, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    attempt_id,
                    assignment_id,
                    configuration_id,
                    requirement,
                    now,
                ],
            )?;
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: "worker_configuration_unavailable",
                    classification,
                    equivalence_key,
                    action,
                    requirement: &requirement,
                },
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::AssignmentReady,
                mandate_revision,
                &serde_json::json!({
                    "assignment_id": assignment_id,
                    "reason": "worker_configuration_unavailable",
                    "configuration_id": configuration_id,
                }),
            )?;
            if let TyrionError::RequiredSkillUnavailable {
                skill_name,
                content_digest,
                message,
                ..
            } = error
            {
                record_required_skill_failure_association(
                    &transaction,
                    commission_id,
                    assignment_id,
                    attempt_id,
                    configuration_id,
                    skill_name,
                    content_digest,
                    message,
                    now_ms,
                )?;
            }
        } else if interrupted {
            transaction.execute(
                "INSERT INTO attention_conditions (
                    id, commission_id, assignment_id, code, requirement, status, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    blocker_code,
                    requirement,
                    now,
                ],
            )?;
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: "interrupted",
                    classification,
                    equivalence_key,
                    action,
                    requirement: &requirement,
                },
            )?;
        } else if action == "retry" {
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: blocker_code.as_str(),
                    classification,
                    equivalence_key,
                    action,
                    requirement: &requirement,
                },
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::AssignmentReady,
                mandate_revision,
                &serde_json::json!({
                    "assignment_id": assignment_id,
                    "reason": "bounded_transient_retry",
                    "prior_attempt_id": attempt_id,
                }),
            )?;
        } else if action == "replan" {
            let next_plan_revision = transaction.query_row(
                "SELECT COALESCE(MAX(revision), 0) + 1 FROM commission_plans
                 WHERE commission_id = ?1",
                [commission_id],
                |row| row.get::<_, i64>(0),
            )?;
            let snapshot = serde_json::json!({
                "reason": "second_equivalent_failure",
                "assignment_id": assignment_id,
                "attempt_id": attempt_id,
                "equivalence_key": equivalence_key,
                "requirement": requirement,
            });
            transaction.execute(
                "INSERT INTO commission_plans (
                    commission_id, revision, source, reason, snapshot_json, created_at
                 ) VALUES (?1, ?2, 'control_plane', 'second equivalent Attempt failure', ?3, ?4)",
                params![
                    commission_id,
                    next_plan_revision,
                    serde_json::to_string(&snapshot)?,
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT INTO attention_conditions (
                    id, commission_id, assignment_id, code, requirement, status, created_at
                 ) VALUES (?1, ?2, ?3, 'replan_required', ?4, 'open', ?5)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    requirement,
                    now,
                ],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO watchdog_findings (
                    id, commission_id, assignment_id, attempt_id, signal, action, details, created_at
                 ) VALUES (?1, ?2, ?3, ?4, 'unhealthy_retry_pattern',
                           'contain_attempt', ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    attempt_id,
                    "A second equivalent failure made another same-configuration retry unsafe.",
                    now,
                ],
            )?;
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: blocker_code.as_str(),
                    classification,
                    equivalence_key,
                    action,
                    requirement: &requirement,
                },
            )?;
            record_event_with_payload(
                &transaction,
                commission_id,
                EventKind::PlanRevised,
                mandate_revision,
                &serde_json::json!({
                    "plan_revision": next_plan_revision,
                    "reason": "second_equivalent_failure",
                    "assignment_id": assignment_id,
                }),
            )?;
        } else {
            transaction.execute(
                "INSERT OR REPLACE INTO blockers (id, commission_id, assignment_id, code, requirement, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    commission_id,
                    assignment_id,
                    blocker_code,
                    requirement,
                    now,
                ],
            )?;
            record_attempt_recovery(
                &transaction,
                AttemptRecovery {
                    commission_id,
                    assignment_id,
                    attempt_id,
                    cause: blocker_code.as_str(),
                    classification,
                    equivalence_key,
                    action,
                    requirement: &requirement,
                },
            )?;
            record_event(
                &transaction,
                commission_id,
                EventKind::AssignmentBlocked,
                mandate_revision,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn control_worker(
    connection: &mut Connection,
    request: &Request,
    commission_id: &str,
    worker_handle: &str,
    message: &str,
    action: WorkerControlAction,
    runtime: &worker::WorkerRuntime,
) -> Result<Value, TyrionError> {
    if worker_handle.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "Worker Handle must not be empty".into(),
        ));
    }
    if message.trim().is_empty() || message.len() > 4096 || message.contains('\0') {
        return Err(TyrionError::InvalidRequest(format!(
            "Worker {} text must contain between 1 and 4096 safe bytes",
            action.as_str()
        )));
    }
    let idempotency_key = mutation_key(request)?;
    let request_hash = request_hash(request)?;
    let expected_revision = request.expected_revision.ok_or_else(|| {
        TyrionError::InvalidRequest(format!(
            "Worker {} requires an expected Commission revision",
            action.as_str()
        ))
    })?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(prior) = prior_result(&transaction, idempotency_key, &request_hash)? {
        return Ok(prior);
    }
    let attachment_id = authenticated_attachment_id(&transaction, request)?;
    ensure_active_attachment(
        &transaction,
        &attachment_id,
        commission_id,
        action.capability(),
    )?;
    let prior_command = transaction
        .query_row(
            "SELECT worker_commands.id, worker_commands.request_hash,
                    worker_commands.status, workers.handle
             FROM worker_commands
             JOIN workers ON workers.id = worker_commands.worker_id
             WHERE worker_commands.idempotency_key = ?1",
            [idempotency_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((_, prior_hash, status, prior_handle)) = &prior_command {
        if prior_hash != &request_hash || prior_handle != worker_handle {
            return Err(TyrionError::IdempotencyConflict);
        }
        if status == "delivered" {
            let result = project_commission(&transaction, commission_id)?;
            save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
            transaction.commit()?;
            return Ok(result);
        }
        if status == "failed" {
            return Err(TyrionError::ControlDenied(format!(
                "the prior Worker {} delivery failed",
                action.as_str()
            )));
        }
    }
    let (status, revision) = transaction
        .query_row(
            "SELECT status, revision FROM commissions WHERE id = ?1",
            [commission_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
    if revision != expected_revision {
        return Err(TyrionError::StaleRevision {
            expected: expected_revision,
            actual: revision,
        });
    }
    if status != CommissionStatus::Active.as_str() {
        return Err(TyrionError::ControlDenied(format!(
            "Commission {commission_id} is {status}"
        )));
    }
    let (worker_id, attempt_id, worker_status, configuration) = transaction
        .query_row(
            "SELECT id, attempt_id, status, configuration_json
             FROM workers WHERE commission_id = ?1 AND handle = ?2",
            params![commission_id, worker_handle],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    serde_json::from_str::<Value>(&row.get::<_, String>(3)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::ControlDenied(format!(
                "Worker Handle {worker_handle} is not part of Commission {commission_id}"
            ))
        })?;
    if worker_status != AttemptStatus::Running.as_str() {
        return Err(TyrionError::ControlDenied(format!(
            "Worker {worker_handle} is {worker_status}"
        )));
    }
    if !worker_configuration_supports_control(&configuration, action) {
        return Err(TyrionError::ControlDenied(format!(
            "Worker {worker_handle} configuration does not support {}",
            action.as_str()
        )));
    }
    let now = unix_timestamp()?;
    let command_id = prior_command
        .as_ref()
        .map(|(id, _, _, _)| id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if prior_command.is_none() {
        transaction.execute(
            "INSERT INTO worker_commands (
                id, commission_id, worker_id, attachment_id, kind, payload_json,
                mandate_revision, status, idempotency_key, request_hash, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9, ?10)",
            params![
                command_id,
                commission_id,
                worker_id,
                attachment_id,
                action.as_str(),
                serde_json::to_string(&serde_json::json!({
                    (action.message_field()): message,
                }))?,
                revision,
                idempotency_key,
                request_hash,
                now,
            ],
        )?;
    }
    transaction.commit()?;

    let delivery = match action {
        WorkerControlAction::Steer => runtime.steer(&attempt_id, &command_id, message),
        WorkerControlAction::Interrupt => runtime.interrupt(&attempt_id, &command_id, message),
    };
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Err(error) = delivery {
        transaction.execute(
            "UPDATE worker_commands SET status = 'failed' WHERE id = ?1 AND status = 'pending'",
            [&command_id],
        )?;
        transaction.commit()?;
        return Err(error);
    }
    let now_ms = unix_timestamp_millis()?;
    let activity = match action {
        WorkerControlAction::Steer => format!("Clarification delivered: {message}"),
        WorkerControlAction::Interrupt => format!("Interruption delivered: {message}"),
    };
    transaction.execute(
        "UPDATE worker_commands SET status = 'delivered' WHERE id = ?1 AND status = 'pending'",
        [&command_id],
    )?;
    transaction.execute(
        "UPDATE workers SET latest_activity = ?2, activity_at_ms = ?3 WHERE id = ?1",
        params![worker_id, activity, now_ms],
    )?;
    record_event_with_payload(
        &transaction,
        commission_id,
        action.event_kind(),
        revision,
        &serde_json::json!({
            "worker_id": worker_id,
            "worker_handle": worker_handle,
            "attempt_id": attempt_id,
            (action.message_field()): message,
            "mandate_revision": revision,
            "mandate_changed": false,
        }),
    )?;
    let result = project_commission(&transaction, commission_id)?;
    save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
    transaction.commit()?;
    Ok(result)
}

fn proposal_plan_or_legacy(
    plan: Option<CommissionPlan>,
    commission_id: &str,
    transaction: &Transaction<'_>,
) -> Result<CommissionPlan, TyrionError> {
    let (
        goal,
        max_storage_bytes,
        max_model_spend_cents,
        max_paid_service_spend_cents,
        worker_requirements_json,
    ) = transaction.query_row(
        "SELECT commissions.goal, resource_ceilings.max_storage_bytes,
                    resource_ceilings.max_model_spend_cents,
                    resource_ceilings.max_paid_service_spend_cents,
                    commissions.worker_requirements_json
             FROM commissions
             JOIN resource_ceilings ON resource_ceilings.commission_id = commissions.id
             WHERE commissions.id = ?1",
        [commission_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    let principal_requirements: WorkerRequirements =
        serde_json::from_str(&worker_requirements_json)?;
    if let Some(mut plan) = plan {
        for assignment in &mut plan.assignments {
            assignment.worker_requirements = merge_worker_requirements(
                &principal_requirements,
                &assignment.worker_requirements,
            )?;
        }
        return Ok(plan);
    }
    let criterion_ids = {
        let mut statement = transaction.prepare(
            "SELECT criterion_id FROM criteria WHERE commission_id = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map([commission_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    Ok(CommissionPlan {
        assignments: vec![PlannedAssignment {
            id: "legacy-assignment".into(),
            goal,
            dependencies: Vec::new(),
            criterion_ids,
            purpose: AssignmentPurpose::CriticalPath,
            read_scopes: Vec::new(),
            write_scopes: load_authorized_paths(transaction, commission_id)?,
            resources: AssignmentResources {
                concurrency_slots: 1,
                max_storage_bytes,
                max_model_spend_cents,
                max_paid_service_spend_cents,
            },
            worker_requirements: principal_requirements,
            competition: None,
        }],
    })
}

fn route_ready_assignments(
    transaction: &Transaction<'_>,
    commission_id: &str,
    worker: &worker::WorkerRuntime,
) -> Result<(), TyrionError> {
    let (execution_json, entry_harness) = transaction.query_row(
        "SELECT commissions.execution_json, attachments.harness
         FROM commissions
         JOIN commission_attachments
           ON commission_attachments.commission_id = commissions.id
          AND commission_attachments.role = 'active'
         JOIN attachments ON attachments.id = commission_attachments.attachment_id
         WHERE commissions.id = ?1",
        [commission_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let execution: ExecutionSpec = serde_json::from_str(&execution_json)?;
    let required_authority_action = match &execution {
        ExecutionSpec::Deterministic => worker::DETERMINISTIC_ACTION,
        ExecutionSpec::CodexGit { .. } => worker::CODEX_GIT_ACTION,
    };
    let assignments = {
        let mut statement = transaction.prepare(
            "SELECT assignments.id, assignments.status,
                    planned_assignments.worker_requirements_json,
                    assignment_metadata.concurrency_slots,
                    assignment_metadata.max_storage_bytes,
                    assignment_metadata.max_model_spend_cents,
                    assignment_metadata.max_paid_service_spend_cents,
                    planned_assignments.read_scopes_json,
                    planned_assignments.write_scopes_json
             FROM assignments
             JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
             JOIN planned_assignments
               ON planned_assignments.commission_id = assignments.commission_id
              AND planned_assignments.logical_id = assignment_metadata.logical_id
             WHERE assignments.commission_id = ?1
               AND assignments.status IN (?2, ?3)
             ORDER BY assignment_metadata.position, assignments.id",
        )?;
        let rows = statement.query_map(
            params![
                commission_id,
                AssignmentStatus::Ready.as_str(),
                AssignmentStatus::AttentionRequired.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    AssignmentResources {
                        concurrency_slots: row.get(3)?,
                        max_storage_bytes: row.get(4)?,
                        max_model_spend_cents: row.get(5)?,
                        max_paid_service_spend_cents: row.get(6)?,
                    },
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (
        assignment_id,
        prior_status,
        requirements_json,
        resources,
        read_scopes_json,
        write_scopes_json,
    ) in assignments
    {
        let mut requirements: WorkerRequirements = serde_json::from_str(&requirements_json)?;
        {
            let mut statement = transaction.prepare(
                "SELECT skill_name, content_digest
                 FROM assignment_skill_defaults
                 WHERE assignment_id = ?1 AND provenance = 'worker'
                 ORDER BY skill_name",
            )?;
            let rows = statement.query_map([&assignment_id], |row| {
                Ok(SelectedSkillVersion {
                    version: SkillVersion {
                        name: row.get(0)?,
                        content_digest: row.get(1)?,
                    },
                    provenance: SkillSelectionProvenance::Worker,
                })
            })?;
            for selected in rows.collect::<Result<Vec<_>, _>>()? {
                if !requirements
                    .selected_skills
                    .iter()
                    .any(|existing| existing.name == selected.name)
                {
                    requirements.selected_skills.push(selected);
                }
            }
        }
        let has_artifact_scopes = !serde_json::from_str::<Vec<String>>(&read_scopes_json)?
            .is_empty()
            || !serde_json::from_str::<Vec<String>>(&write_scopes_json)?.is_empty();
        let required_authority_scope_types = match execution {
            ExecutionSpec::CodexGit { .. } => vec!["repository", "path", "action"],
            ExecutionSpec::Deterministic if has_artifact_scopes => vec!["path", "action"],
            ExecutionSpec::Deterministic => vec!["action"],
        };
        let unavailable_configuration_ids = {
            let mut statement = transaction.prepare(
                "SELECT configuration_id FROM worker_configuration_failures
                 WHERE assignment_id = ?1
                 UNION
                 SELECT attempts.worker_configuration
                 FROM attempts
                 JOIN attempt_recoveries ON attempt_recoveries.attempt_id = attempts.id
                 WHERE attempts.assignment_id = ?1
                   AND attempt_recoveries.action = 'replan'
                 ORDER BY 1",
            )?;
            let rows = statement.query_map([&assignment_id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<std::collections::HashSet<_>, _>>()?
        };
        let route = worker.route(
            &requirements,
            &resources,
            required_authority_action,
            &required_authority_scope_types,
            &entry_harness,
            &unavailable_configuration_ids,
        )?;
        let status = route["status"].as_str().ok_or_else(|| {
            TyrionError::InvalidRequest("Worker route decision is missing its status".into())
        })?;
        let selected_configuration_json = (!route["selected_configuration"].is_null())
            .then(|| serde_json::to_string(&route["selected_configuration"]))
            .transpose()?;
        transaction.execute(
            "INSERT OR REPLACE INTO assignment_routes (
                assignment_id, status, selected_configuration_json, rationale_json, decided_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                assignment_id,
                status,
                selected_configuration_json,
                serde_json::to_string(&route["rationale"])?,
                unix_timestamp()?,
            ],
        )?;
        let has_non_routing_attention = prior_status
            == AssignmentStatus::AttentionRequired.as_str()
            && transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM attention_conditions
                    WHERE assignment_id = ?1 AND status = 'open'
                      AND code NOT IN (
                          'worker_configuration_ineligible',
                          'worker_configuration_unavailable'
                      )
                 )",
                [&assignment_id],
                |row| row.get::<_, bool>(0),
            )?;
        if has_non_routing_attention {
            continue;
        }
        if status == "selected" && prior_status == AssignmentStatus::AttentionRequired.as_str() {
            transaction.execute(
                "UPDATE assignments SET status = ?2 WHERE id = ?1",
                params![assignment_id, AssignmentStatus::Ready.as_str()],
            )?;
            transaction.execute(
                "UPDATE attention_conditions
                 SET status = 'resolved', resolved_at = ?2
                 WHERE assignment_id = ?1 AND status = 'open'",
                params![assignment_id, unix_timestamp()?],
            )?;
        } else if status == "attention_required" {
            let requirement = route["rationale"]["attention_requirement"]
                .as_str()
                .ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "attention-required Worker route is missing its requirement".into(),
                    )
                })?;
            let code = if route["rationale"]["preferred_unavailable_configuration"].is_null() {
                "worker_configuration_ineligible"
            } else {
                "worker_configuration_unavailable"
            };
            transaction.execute(
                "UPDATE assignments SET status = ?2 WHERE id = ?1",
                params![assignment_id, AssignmentStatus::AttentionRequired.as_str()],
            )?;
            let updated = transaction.execute(
                "UPDATE attention_conditions
                 SET code = ?2, requirement = ?3
                 WHERE assignment_id = ?1 AND status = 'open'",
                params![assignment_id, code, requirement],
            )?;
            if updated == 0 {
                transaction.execute(
                    "INSERT INTO attention_conditions (
                    id, commission_id, assignment_id, code, requirement, status, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
                    params![
                        Uuid::new_v4().to_string(),
                        commission_id,
                        assignment_id,
                        code,
                        requirement,
                        unix_timestamp()?,
                    ],
                )?;
            }
        }
    }
    Ok(())
}

fn initialize_commission_plan(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    plan: &CommissionPlan,
    legacy: bool,
    created_at: i64,
) -> Result<(), TyrionError> {
    let plan_revision = 1_i64;
    transaction.execute(
        "INSERT INTO commission_plans (
            commission_id, revision, source, reason, snapshot_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            commission_id,
            plan_revision,
            if legacy {
                "control_plane"
            } else {
                "entry_model"
            },
            "initial decomposition exposed the first safe Execution Frontier",
            serde_json::to_string(plan)?,
            created_at,
        ],
    )?;
    record_event_with_payload(
        transaction,
        commission_id,
        EventKind::PlanRevised,
        mandate_revision,
        &serde_json::json!({
            "plan_revision": plan_revision,
            "source": if legacy { "control_plane" } else { "entry_model" },
            "reason": "initial_decomposition",
        }),
    )?;

    for (position, assignment) in plan.assignments.iter().enumerate() {
        let competition = assignment.competition.as_ref();
        transaction.execute(
            "INSERT INTO planned_assignments (
                commission_id, logical_id, position, goal, purpose,
                read_scopes_json, write_scopes_json, concurrency_slots,
                max_storage_bytes, max_model_spend_cents,
                max_paid_service_spend_cents, worker_requirements_json, competition_group,
                competition_uncertainty, competition_rule, created_plan_revision
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
             )",
            params![
                commission_id,
                assignment.id,
                position as i64,
                assignment.goal,
                assignment.purpose.as_str(),
                serde_json::to_string(&assignment.read_scopes)?,
                serde_json::to_string(&assignment.write_scopes)?,
                assignment.resources.concurrency_slots,
                assignment.resources.max_storage_bytes,
                assignment.resources.max_model_spend_cents,
                assignment.resources.max_paid_service_spend_cents,
                serde_json::to_string(&assignment.worker_requirements)?,
                competition.map(|item| item.group.as_str()),
                competition.map(|item| item.uncertainty.as_str()),
                competition.map(|item| item.comparison_rule.as_str()),
                plan_revision,
            ],
        )?;
        for (dependency_position, dependency) in assignment.dependencies.iter().enumerate() {
            transaction.execute(
                "INSERT INTO planned_assignment_dependencies (
                    commission_id, assignment_logical_id, dependency_logical_id, position
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    commission_id,
                    assignment.id,
                    dependency,
                    dependency_position as i64,
                ],
            )?;
        }
        for (criterion_position, criterion_id) in assignment.criterion_ids.iter().enumerate() {
            transaction.execute(
                "INSERT INTO planned_assignment_criteria (
                    commission_id, assignment_logical_id, criterion_id, position
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    commission_id,
                    assignment.id,
                    criterion_id,
                    criterion_position as i64,
                ],
            )?;
        }
    }

    for assignment in plan
        .assignments
        .iter()
        .filter(|assignment| assignment.dependencies.is_empty())
    {
        insert_ready_assignment(
            transaction,
            commission_id,
            &assignment.id,
            plan_revision,
            mandate_revision,
            legacy,
            created_at,
        )?;
    }
    Ok(())
}

fn insert_ready_assignment(
    transaction: &Transaction<'_>,
    commission_id: &str,
    logical_id: &str,
    plan_revision: i64,
    mandate_revision: i64,
    legacy: bool,
    created_at: i64,
) -> Result<String, TyrionError> {
    let assignment_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            assignment_id,
            commission_id,
            plan_revision,
            AssignmentStatus::Ready.as_str(),
            created_at,
        ],
    )?;
    transaction.execute(
        "INSERT INTO assignment_metadata (
            assignment_id, commission_id, logical_id, position, goal, purpose,
            read_scopes_json, write_scopes_json, concurrency_slots,
            max_storage_bytes, max_model_spend_cents,
            max_paid_service_spend_cents, competition_group,
            competition_uncertainty, competition_rule, legacy
         )
         SELECT ?1, ?2, logical_id, position, goal, purpose, read_scopes_json,
                write_scopes_json, concurrency_slots, max_storage_bytes,
                max_model_spend_cents, max_paid_service_spend_cents,
                competition_group, competition_uncertainty, competition_rule, ?4
         FROM planned_assignments
         WHERE commission_id = ?2 AND logical_id = ?3",
        params![assignment_id, commission_id, logical_id, legacy],
    )?;
    let requirements: WorkerRequirements = transaction.query_row(
        "SELECT worker_requirements_json FROM planned_assignments
         WHERE commission_id = ?1 AND logical_id = ?2",
        params![commission_id, logical_id],
        |row| {
            let encoded = row.get::<_, String>(0)?;
            serde_json::from_str(&encoded).map_err(|error| invalid_json_column(0, error))
        },
    )?;
    let principal_requirements: WorkerRequirements = transaction.query_row(
        "SELECT worker_requirements_json FROM commissions WHERE id = ?1",
        [commission_id],
        |row| {
            let encoded = row.get::<_, String>(0)?;
            serde_json::from_str(&encoded).map_err(|error| invalid_json_column(0, error))
        },
    )?;
    for skill in &requirements.skills {
        insert_assignment_skill_default(
            transaction,
            &assignment_id,
            skill,
            "required",
            if principal_requirements
                .skills
                .iter()
                .any(|principal| principal == skill)
            {
                "principal"
            } else {
                "plan"
            },
            plan_revision,
            created_at,
        )?;
    }
    record_event_with_payload(
        transaction,
        commission_id,
        EventKind::AssignmentReady,
        mandate_revision,
        &serde_json::json!({
            "assignment_id": assignment_id,
            "logical_id": logical_id,
            "plan_revision": plan_revision,
        }),
    )?;
    Ok(assignment_id)
}

fn insert_assignment_skill_default(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    skill: &SkillVersion,
    requirement: &str,
    provenance: &str,
    plan_revision: i64,
    selected_at: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO assignment_skill_defaults (
            assignment_id, skill_name, content_digest, requirement, provenance,
            plan_revision, delegation, selected_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'native_unchanged', ?7)",
        params![
            assignment_id,
            skill.name,
            skill.content_digest,
            requirement,
            provenance,
            plan_revision,
            selected_at,
        ],
    )?;
    Ok(())
}

fn load_attempt_skill_defaults(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    selected_configuration: &Value,
) -> Result<Vec<worker::AssignmentSkillDefault>, TyrionError> {
    let mut defaults = load_assignment_skill_defaults(transaction, assignment_id)?;
    let requirements_json = transaction.query_row(
        "SELECT planned_assignments.worker_requirements_json
         FROM assignment_metadata
         JOIN planned_assignments
           ON planned_assignments.commission_id = assignment_metadata.commission_id
          AND planned_assignments.logical_id = assignment_metadata.logical_id
         WHERE assignment_metadata.assignment_id = ?1",
        [assignment_id],
        |row| row.get::<_, String>(0),
    )?;
    let requirements: WorkerRequirements = serde_json::from_str(&requirements_json)?;
    let mut selected = requirements
        .selected_skills
        .iter()
        .map(|skill| (skill.version(), skill.provenance.as_str()))
        .collect::<Vec<_>>();
    selected.extend(
        serde_json::from_value::<Vec<SkillVersion>>(
            selected_configuration["selected_skills"].clone(),
        )?
        .into_iter()
        .map(|skill| (skill, SkillSelectionProvenance::Worker.as_str())),
    );
    for (skill, provenance) in selected {
        match defaults.iter().find(|default| default.name == skill.name) {
            Some(default) if default.content_digest != skill.content_digest => {
                return Err(TyrionError::InvalidRequest(format!(
                    "invoked Skill Version {} conflicts with the Assignment default",
                    skill.name
                )));
            }
            Some(_) => {}
            None => defaults.push(worker::AssignmentSkillDefault {
                version: skill,
                requirement: worker::AssignmentSkillRequirement::Selected,
                provenance: SkillSelectionProvenance::parse(provenance).ok_or_else(|| {
                    TyrionError::InvalidRequest(format!(
                        "stored Skill provenance {provenance} is invalid"
                    ))
                })?,
                delegation: worker::NativeSkillDelegation::NativeUnchanged,
            }),
        }
    }
    defaults.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(defaults)
}

fn record_observed_skill_defaults(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    launch_defaults: &[worker::AssignmentSkillDefault],
    invoked_versions: &[SkillVersion],
    plan_revision: i64,
    invoked_at: i64,
) -> Result<(), TyrionError> {
    for skill in invoked_versions {
        let launch_default = launch_defaults
            .iter()
            .find(|default| default.name == skill.name);
        let (requirement, provenance) = match launch_default {
            Some(default) if default.content_digest == skill.content_digest => {
                (default.requirement.as_str(), default.provenance.as_str())
            }
            Some(_) => {
                return Err(TyrionError::InvalidRequest(format!(
                    "invoked Skill Version {} conflicts with the Assignment default",
                    skill.name
                )))
            }
            None => ("selected", SkillSelectionProvenance::Worker.as_str()),
        };
        let pinned_digest = transaction
            .query_row(
                "SELECT content_digest FROM assignment_skill_defaults
                 WHERE assignment_id = ?1 AND skill_name = ?2",
                params![assignment_id, skill.name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match pinned_digest {
            Some(content_digest) if content_digest != skill.content_digest => {
                return Err(TyrionError::InvalidRequest(format!(
                    "invoked Skill Version {} conflicts with the Assignment default",
                    skill.name
                )))
            }
            Some(_) => {}
            None => insert_assignment_skill_default(
                transaction,
                assignment_id,
                skill,
                requirement,
                provenance,
                plan_revision,
                invoked_at,
            )?,
        }
    }
    Ok(())
}

fn load_assignment_skill_defaults(
    transaction: &Transaction<'_>,
    assignment_id: &str,
) -> Result<Vec<worker::AssignmentSkillDefault>, TyrionError> {
    let mut statement = transaction.prepare(
        "SELECT skill_name, content_digest, requirement, provenance, delegation
         FROM assignment_skill_defaults
         WHERE assignment_id = ?1 ORDER BY skill_name",
    )?;
    let rows = statement.query_map([assignment_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(
            |(name, content_digest, requirement, provenance, delegation)| {
                Ok(worker::AssignmentSkillDefault {
                    version: SkillVersion {
                        name,
                        content_digest,
                    },
                    requirement: worker::AssignmentSkillRequirement::parse(&requirement)
                        .ok_or_else(|| {
                            TyrionError::InvalidRequest(format!(
                                "stored Skill requirement {requirement} is invalid"
                            ))
                        })?,
                    provenance: SkillSelectionProvenance::parse(&provenance).ok_or_else(|| {
                        TyrionError::InvalidRequest(format!(
                            "stored Skill provenance {provenance} is invalid"
                        ))
                    })?,
                    delegation: worker::NativeSkillDelegation::parse(&delegation).ok_or_else(
                        || {
                            TyrionError::InvalidRequest(format!(
                                "stored Skill delegation {delegation} is invalid"
                            ))
                        },
                    )?,
                })
            },
        )
        .collect()
}

fn load_criteria(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<Vec<worker::CriterionDefinition>, TyrionError> {
    let mut statement = transaction.prepare(
        "SELECT criterion_id, required_evidence, verifier_type, verification_depth,
                verifier_configuration, verification_environment, verifier_kind, expected
         FROM criteria WHERE commission_id = ?1 ORDER BY position",
    )?;
    let rows = statement.query_map([commission_id], stored_criterion)?;
    rows.map(|row| {
        let StoredCriterion {
            id,
            required_evidence,
            verifier_type,
            verification_depth,
            verifier_configuration,
            verification_environment,
            verifier_kind,
            expected,
        } = row?;
        let verifier = match verifier_kind.as_str() {
            "exact_match" => Verifier::ExactMatch { expected },
            "command" => Verifier::Command {
                argv: serde_json::from_str(&expected)?,
            },
            "prompt" => Verifier::Prompt { prompt: expected },
            _ => {
                return Err(TyrionError::InvalidRequest(format!(
                    "unsupported persisted verifier {verifier_kind}"
                )))
            }
        };
        Ok(worker::CriterionDefinition {
            id,
            required_evidence,
            verifier_type,
            verification_depth,
            verifier_configuration,
            verification_environment,
            verifier,
        })
    })
    .collect()
}

fn load_assignment_criteria(
    transaction: &Transaction<'_>,
    commission_id: &str,
    logical_id: &str,
    legacy: bool,
) -> Result<Vec<worker::CriterionDefinition>, TyrionError> {
    let criteria = load_criteria(transaction, commission_id)?;
    if legacy {
        return Ok(criteria);
    }
    let criterion_ids = {
        let mut statement = transaction.prepare(
            "SELECT criterion_id FROM planned_assignment_criteria
             WHERE commission_id = ?1 AND assignment_logical_id = ?2
             ORDER BY position",
        )?;
        let rows = statement.query_map(params![commission_id, logical_id], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<HashSet<_>, _>>()?
    };
    Ok(criteria
        .into_iter()
        .filter(|criterion| criterion_ids.contains(&criterion.id))
        .collect())
}

fn string_vec_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Vec<String>> {
    let encoded = row.get::<_, String>(index)?;
    serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn competition(
    group: &Option<String>,
    uncertainty: &Option<String>,
    rule: &Option<String>,
) -> Option<Competition> {
    Some(Competition {
        group: group.clone()?,
        uncertainty: uncertainty.clone()?,
        rule: rule.clone()?,
    })
}

fn load_authorized_paths(
    connection: &Connection,
    commission_id: &str,
) -> Result<Vec<String>, TyrionError> {
    load_authority_scope(connection, commission_id, AuthorityScopeType::Path)
}

fn load_authority(
    connection: &Connection,
    commission_id: &str,
) -> Result<AuthorityEnvelope, TyrionError> {
    Ok(AuthorityEnvelope {
        repositories: load_authority_scope(
            connection,
            commission_id,
            AuthorityScopeType::Repository,
        )?,
        paths: load_authority_scope(connection, commission_id, AuthorityScopeType::Path)?,
        actions: load_authority_scope(connection, commission_id, AuthorityScopeType::Action)?,
        destinations: load_authority_scope(
            connection,
            commission_id,
            AuthorityScopeType::Destination,
        )?,
        effects: load_authority_scope(connection, commission_id, AuthorityScopeType::Effect)?,
    })
}

fn load_resource_ceilings(
    connection: &Connection,
    commission_id: &str,
) -> Result<ResourceCeilings, TyrionError> {
    Ok(connection.query_row(
        "SELECT max_attempts, max_elapsed_seconds, max_worker_concurrency,
                max_storage_bytes, max_model_spend_cents,
                max_paid_service_spend_cents
         FROM resource_ceilings WHERE commission_id = ?1",
        [commission_id],
        |row| {
            Ok(ResourceCeilings {
                max_attempts: row.get(0)?,
                max_elapsed_seconds: row.get(1)?,
                max_worker_concurrency: row.get(2)?,
                max_storage_bytes: row.get(3)?,
                max_model_spend_cents: row.get(4)?,
                max_paid_service_spend_cents: row.get(5)?,
            })
        },
    )?)
}

fn validate_commission_amendment(amendment: &CommissionAmendment) -> Result<(), TyrionError> {
    if amendment.reason.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "Commission Amendment reason must not be empty".into(),
        ));
    }
    let named_scopes = [
        ("repository", &amendment.authority.repositories),
        ("path", &amendment.authority.paths),
        ("action", &amendment.authority.actions),
        ("destination", &amendment.authority.destinations),
        ("effect", &amendment.authority.effects),
    ];
    for (name, values) in named_scopes {
        let mut unique = HashSet::new();
        for value in values {
            if value.trim().is_empty() || value.contains('\0') || !unique.insert(value) {
                return Err(TyrionError::InvalidRequest(format!(
                    "Commission Amendment {name} scopes must be non-empty and unique"
                )));
            }
        }
    }
    for repository in &amendment.authority.repositories {
        if !Path::new(repository).is_absolute() || fs::canonicalize(repository).is_err() {
            return Err(TyrionError::InvalidRequest(
                "Commission Amendment repositories must be resolvable absolute paths".into(),
            ));
        }
    }
    for path in &amendment.authority.paths {
        validate_relative_scope(path)?;
    }
    if amendment
        .authority
        .effects
        .iter()
        .any(|effect| effect == "filesystem.write")
        && (!amendment
            .authority
            .actions
            .iter()
            .any(|action| action == "filesystem.write")
            || !amendment
                .authority
                .destinations
                .iter()
                .any(|destination| destination == "local"))
    {
        return Err(TyrionError::InvalidRequest(
            "filesystem.write effect authority requires its exact action and local destination"
                .into(),
        ));
    }
    let ceilings = &amendment.resource_ceilings;
    if ceilings.max_attempts == 0
        || ceilings.max_elapsed_seconds == 0
        || ceilings.max_worker_concurrency == 0
        || ceilings.max_storage_bytes == 0
        || ceilings.max_elapsed_seconds > i64::MAX as u64
        || ceilings.max_storage_bytes > i64::MAX as u64
        || ceilings.max_model_spend_cents > i64::MAX as u64
        || ceilings.max_paid_service_spend_cents > i64::MAX as u64
    {
        return Err(TyrionError::InvalidRequest(
            "Commission Amendment resource ceilings must be positive and fit SQLite integers"
                .into(),
        ));
    }
    Ok(())
}

fn commission_amendment_diff(
    current_authority: &AuthorityEnvelope,
    proposed_authority: &AuthorityEnvelope,
    current_ceilings: &ResourceCeilings,
    proposed_ceilings: &ResourceCeilings,
) -> Value {
    let mut resource_changes = serde_json::Map::new();
    insert_resource_change(
        &mut resource_changes,
        "max_attempts",
        current_ceilings.max_attempts,
        proposed_ceilings.max_attempts,
    );
    insert_resource_change(
        &mut resource_changes,
        "max_elapsed_seconds",
        current_ceilings.max_elapsed_seconds,
        proposed_ceilings.max_elapsed_seconds,
    );
    insert_resource_change(
        &mut resource_changes,
        "max_worker_concurrency",
        current_ceilings.max_worker_concurrency,
        proposed_ceilings.max_worker_concurrency,
    );
    insert_resource_change(
        &mut resource_changes,
        "max_storage_bytes",
        current_ceilings.max_storage_bytes,
        proposed_ceilings.max_storage_bytes,
    );
    insert_resource_change(
        &mut resource_changes,
        "max_model_spend_cents",
        current_ceilings.max_model_spend_cents,
        proposed_ceilings.max_model_spend_cents,
    );
    insert_resource_change(
        &mut resource_changes,
        "max_paid_service_spend_cents",
        current_ceilings.max_paid_service_spend_cents,
        proposed_ceilings.max_paid_service_spend_cents,
    );
    serde_json::json!({
        "changed": current_authority != proposed_authority || current_ceilings != proposed_ceilings,
        "authority": {
            "repositories": authority_list_diff(
                &current_authority.repositories,
                &proposed_authority.repositories,
            ),
            "paths": authority_list_diff(&current_authority.paths, &proposed_authority.paths),
            "actions": authority_list_diff(&current_authority.actions, &proposed_authority.actions),
            "destinations": authority_list_diff(
                &current_authority.destinations,
                &proposed_authority.destinations,
            ),
            "effects": authority_list_diff(&current_authority.effects, &proposed_authority.effects),
        },
        "resource_ceilings": resource_changes,
    })
}

fn authority_list_diff(current: &[String], proposed: &[String]) -> Value {
    serde_json::json!({
        "added": proposed
            .iter()
            .filter(|value| !current.contains(value))
            .collect::<Vec<_>>(),
        "removed": current
            .iter()
            .filter(|value| !proposed.contains(value))
            .collect::<Vec<_>>(),
    })
}

fn insert_resource_change<T: Serialize + PartialEq>(
    changes: &mut serde_json::Map<String, Value>,
    name: &str,
    before: T,
    after: T,
) {
    if before != after {
        changes.insert(
            name.to_owned(),
            serde_json::json!({"before": before, "after": after}),
        );
    }
}

fn validate_amended_execution_authority(
    connection: &Connection,
    commission_id: &str,
    authority: &AuthorityEnvelope,
) -> Result<(), TyrionError> {
    let execution_json = connection.query_row(
        "SELECT execution_json FROM commissions WHERE id = ?1",
        [commission_id],
        |row| row.get::<_, String>(0),
    )?;
    let required_action = match serde_json::from_str::<ExecutionSpec>(&execution_json)? {
        ExecutionSpec::Deterministic => worker::DETERMINISTIC_ACTION,
        ExecutionSpec::CodexGit { .. } => worker::CODEX_GIT_ACTION,
    };
    if !authority
        .actions
        .iter()
        .any(|action| action == required_action)
    {
        return Err(TyrionError::InvalidRequest(format!(
            "Commission Amendment cannot remove required execution action {required_action}"
        )));
    }
    Ok(())
}

fn validate_amended_resource_ceilings(
    connection: &Connection,
    commission_id: &str,
    ceilings: &ResourceCeilings,
) -> Result<(), TyrionError> {
    let (attempts, active_concurrency, active_storage, model_spend, paid_spend) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM attempts
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = ?1),
                COALESCE(SUM(CASE WHEN status = 'active' THEN concurrency_slots ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'active' THEN storage_bytes ELSE 0 END), 0),
                COALESCE(SUM(model_spend_cents), 0),
                COALESCE(SUM(paid_service_spend_cents), 0)
             FROM resource_reservations WHERE commission_id = ?1",
            [commission_id],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )?;
    if attempts > ceilings.max_attempts
        || active_concurrency > ceilings.max_worker_concurrency
        || active_storage > ceilings.max_storage_bytes
        || model_spend > ceilings.max_model_spend_cents
        || paid_spend > ceilings.max_paid_service_spend_cents
    {
        return Err(TyrionError::InvalidRequest(
            "Commission Amendment resource ceilings cannot fall below committed use".into(),
        ));
    }
    Ok(())
}

fn query_active_lease_impact(
    connection: &Connection,
    commission_id: &str,
) -> Result<Vec<Value>, TyrionError> {
    let mut statement = connection.prepare(
        "SELECT worker_leases.id, attempts.id, worker_leases.mandate_revision
         FROM worker_leases
         JOIN attempts ON attempts.id = worker_leases.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         WHERE assignments.commission_id = ?1 AND worker_leases.status = 'active'
         ORDER BY attempts.started_at, attempts.id",
    )?;
    let rows = statement.query_map([commission_id], |row| {
        Ok(serde_json::json!({
            "worker_lease_id": row.get::<_, String>(0)?,
            "attempt_id": row.get::<_, String>(1)?,
            "current_mandate_revision": row.get::<_, i64>(2)?,
            "required_action": "revalidate_before_mandate_change",
        }))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn query_active_leases(
    connection: &Connection,
    commission_id: &str,
) -> Result<Vec<(String, String)>, TyrionError> {
    let mut statement = connection.prepare(
        "SELECT worker_leases.id, attempts.id
         FROM worker_leases
         JOIN attempts ON attempts.id = worker_leases.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         WHERE assignments.commission_id = ?1 AND worker_leases.status = 'active'
         ORDER BY attempts.started_at, attempts.id",
    )?;
    let rows = statement.query_map([commission_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn query_open_operation_ids(
    connection: &Connection,
    commission_id: &str,
) -> Result<Vec<String>, TyrionError> {
    let mut statement = connection.prepare(
        "SELECT id FROM operation_requests
         WHERE commission_id = ?1
           AND status IN ('approval_required', 'authorized', 'started')
         ORDER BY proposed_at, id",
    )?;
    let rows = statement.query_map([commission_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn replace_authority(
    transaction: &Transaction<'_>,
    commission_id: &str,
    authority: &AuthorityEnvelope,
) -> Result<(), TyrionError> {
    transaction.execute(
        "DELETE FROM authority_scopes WHERE commission_id = ?1",
        [commission_id],
    )?;
    let scopes = [
        (AuthorityScopeType::Repository, &authority.repositories),
        (AuthorityScopeType::Path, &authority.paths),
        (AuthorityScopeType::Action, &authority.actions),
        (AuthorityScopeType::Destination, &authority.destinations),
        (AuthorityScopeType::Effect, &authority.effects),
    ];
    for (scope_type, values) in scopes {
        for (position, value) in values.iter().enumerate() {
            transaction.execute(
                "INSERT INTO authority_scopes (commission_id, scope_type, position, value)
                 VALUES (?1, ?2, ?3, ?4)",
                params![commission_id, scope_type.as_str(), position as i64, value],
            )?;
        }
    }
    Ok(())
}

fn replace_resource_ceilings(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ceilings: &ResourceCeilings,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE resource_ceilings SET
            max_attempts = ?2, max_elapsed_seconds = ?3, max_worker_concurrency = ?4,
            max_storage_bytes = ?5, max_model_spend_cents = ?6,
            max_paid_service_spend_cents = ?7
         WHERE commission_id = ?1",
        params![
            commission_id,
            ceilings.max_attempts,
            ceilings.max_elapsed_seconds,
            ceilings.max_worker_concurrency,
            ceilings.max_storage_bytes,
            ceilings.max_model_spend_cents,
            ceilings.max_paid_service_spend_cents,
        ],
    )?;
    Ok(())
}

fn copy_current_criterion_versions(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO criterion_versions (
            commission_id, mandate_revision, criterion_id, position, description,
            required_evidence, verifier_type, verification_depth, verifier_configuration,
            verification_environment, verifier_kind, expected
         )
         SELECT commission_id, ?2, criterion_id, position, description, required_evidence,
                verifier_type, verification_depth, verifier_configuration,
                verification_environment, verifier_kind, expected
         FROM criteria WHERE commission_id = ?1",
        params![commission_id, mandate_revision],
    )?;
    Ok(())
}

fn attempt_resources_fit(
    connection: &Connection,
    commission_id: &str,
    attempt_id: &str,
    ceilings: &ResourceCeilings,
) -> Result<bool, TyrionError> {
    Ok(connection
        .query_row(
            "SELECT concurrency_slots, storage_bytes, model_spend_cents,
                    paid_service_spend_cents
             FROM resource_reservations
             WHERE commission_id = ?1 AND attempt_id = ?2",
            params![commission_id, attempt_id],
            |row| {
                Ok(row.get::<_, u32>(0)? <= ceilings.max_worker_concurrency
                    && row.get::<_, u64>(1)? <= ceilings.max_storage_bytes
                    && row.get::<_, u64>(2)? <= ceilings.max_model_spend_cents
                    && row.get::<_, u64>(3)? <= ceilings.max_paid_service_spend_cents)
            },
        )
        .optional()?
        .unwrap_or(false))
}

fn load_authority_scope(
    connection: &Connection,
    commission_id: &str,
    scope_type: AuthorityScopeType,
) -> Result<Vec<String>, TyrionError> {
    let mut statement = connection.prepare(
        "SELECT value FROM authority_scopes
         WHERE commission_id = ?1 AND scope_type = ?2 ORDER BY position",
    )?;
    let rows = statement.query_map(params![commission_id, scope_type.as_str()], |row| {
        row.get(0)
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn attempt_authority_is_current(
    connection: &Connection,
    commission_id: &str,
    attempt_id: &str,
    execution: &ExecutionSpec,
) -> Result<bool, TyrionError> {
    let authority = load_authority(connection, commission_id)?;
    let required_action = match execution {
        ExecutionSpec::Deterministic => worker::DETERMINISTIC_ACTION,
        ExecutionSpec::CodexGit { .. } => worker::CODEX_GIT_ACTION,
    };
    if !authority
        .actions
        .iter()
        .any(|action| action == required_action)
    {
        return Ok(false);
    }
    if let ExecutionSpec::CodexGit { repository, .. } = execution {
        if !authority
            .repositories
            .iter()
            .any(|authorized| authorized == repository)
            || authority.paths.is_empty()
        {
            return Ok(false);
        }
    }
    let declared_write_scopes_json = connection.query_row(
        "SELECT assignment_metadata.write_scopes_json
         FROM attempts
         JOIN assignments ON assignments.id = attempts.assignment_id
         JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
         WHERE attempts.id = ?1 AND assignments.commission_id = ?2",
        params![attempt_id, commission_id],
        |row| row.get::<_, String>(0),
    )?;
    let declared_write_scopes: Vec<String> = serde_json::from_str(&declared_write_scopes_json)?;
    if declared_write_scopes.iter().any(|scope| {
        !authority
            .paths
            .iter()
            .any(|authorized| path_is_within_scope(scope, authorized))
    }) {
        return Ok(false);
    }
    let observed = {
        let mut statement = connection.prepare(
            "SELECT changed_paths_json, known_effects_json
             FROM results WHERE attempt_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([attempt_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (changed_paths_json, known_effects_json) in observed {
        let changed_paths: Vec<String> = serde_json::from_str(&changed_paths_json)?;
        let known_effects: Vec<String> = serde_json::from_str(&known_effects_json)?;
        if changed_paths.iter().any(|path| {
            !authority
                .paths
                .iter()
                .any(|authorized| path_is_within_scope(path, authorized))
        }) || known_effects.iter().any(|effect| {
            !authority
                .effects
                .iter()
                .any(|authorized| authorized == effect)
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn load_comparison_candidates(
    transaction: &Transaction<'_>,
    commission_id: &str,
    competition_group: Option<&str>,
) -> Result<Vec<worker::ComparisonCandidate>, TyrionError> {
    let Some(competition_group) = competition_group else {
        return Ok(Vec::new());
    };
    let mut statement = transaction.prepare(
        "SELECT results.id, results.artifact_revision, results.output,
                results.changed_paths_json, results.verification_outcomes_json,
                results.artifacts_json
         FROM results
         JOIN attempts ON attempts.id = results.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
         WHERE assignments.commission_id = ?1
           AND assignment_metadata.competition_group = ?2
           AND assignment_metadata.purpose != 'reconciliation'
           AND results.status = ?3
         ORDER BY assignment_metadata.position, results.id",
    )?;
    let rows = statement.query_map(
        params![
            commission_id,
            competition_group,
            ResultStatus::Candidate.as_str()
        ],
        |row| {
            let artifacts = serde_json::from_str::<Value>(&row.get::<_, String>(5)?)
                .map_err(|error| invalid_json_column(5, error))?;
            let bundle_path = artifacts
                .as_array()
                .into_iter()
                .flatten()
                .find(|artifact| artifact["kind"] == "candidate_git_bundle")
                .and_then(|artifact| artifact["path"].as_str())
                .ok_or_else(|| {
                    invalid_json_column(
                        5,
                        serde_json::Error::io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "candidate Result has no candidate_git_bundle artifact",
                        )),
                    )
                })?;
            Ok(worker::ComparisonCandidate {
                result_id: row.get(0)?,
                artifact_revision: row.get(1)?,
                summary: row.get(2)?,
                changed_paths: serde_json::from_str(&row.get::<_, String>(3)?)
                    .map_err(|error| invalid_json_column(3, error))?,
                verification_outcomes: serde_json::from_str(&row.get::<_, String>(4)?)
                    .map_err(|error| invalid_json_column(4, error))?,
                bundle_path: Path::new(bundle_path).to_owned(),
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn invalid_json_column(index: usize, error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}

fn record_evidence(
    transaction: &Transaction<'_>,
    commission_id: &str,
    result_id: &str,
    mandate_revision: i64,
    artifact_revision: &str,
    verification: &[worker::VerificationRecord],
) -> Result<(), TyrionError> {
    for record in verification {
        let outcome = record.outcome;
        transaction.execute(
            "INSERT INTO evidence (
                id, commission_id, criterion_id, result_id, mandate_revision,
                artifact_revision, evidence_type, verifier_type, scope,
                verification_attempt_id, verifier_identity, verifier_configuration,
                verifier_kind, procedure_json, environment, outcome, observed, expected,
                material_contradiction, defect, producer_attempt_id, created_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22
             )",
            params![
                Uuid::new_v4().to_string(),
                commission_id,
                record.criterion_id,
                result_id,
                mandate_revision,
                artifact_revision,
                record.evidence_type,
                record.verifier_type.as_str(),
                record.scope.as_str(),
                record.verification_attempt_id,
                record.verifier_identity,
                record.verifier_configuration,
                record.verifier_kind.as_str(),
                serde_json::to_string(&record.procedure)?,
                record.environment,
                record.outcome.as_str(),
                record.observed,
                record.expected,
                record.material_contradiction,
                record.defect.map(|defect| defect.as_str()),
                record.producer_attempt_id,
                unix_timestamp()?,
            ],
        )?;
        transaction.execute(
            "UPDATE criteria SET status = ?3 WHERE commission_id = ?1 AND criterion_id = ?2",
            params![
                commission_id,
                record.criterion_id,
                outcome.criterion_status().as_str(),
            ],
        )?;
        record_event(
            transaction,
            commission_id,
            EventKind::EvidenceRecorded,
            mandate_revision,
        )?;
    }
    Ok(())
}

fn open_principal_verification_gates(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    opened_at: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO verification_gates (
            commission_id, criterion_id, mandate_revision, status, opened_at
         )
         SELECT commission_id, criterion_id, ?2, 'open', ?3
         FROM criteria
         WHERE commission_id = ?1 AND verifier_type = ?4",
        params![
            commission_id,
            mandate_revision,
            opened_at,
            VerifierType::Principal.as_str()
        ],
    )?;
    Ok(())
}

fn refresh_principal_verification_gate(
    transaction: &Transaction<'_>,
    commission_id: &str,
    criterion: &StoredCriterion,
    mandate_revision: i64,
) -> Result<(), TyrionError> {
    if criterion.verifier_type != VerifierType::Principal {
        return Ok(());
    }
    let criterion_passed = transaction.query_row(
        "SELECT status = ?3 FROM criteria WHERE commission_id = ?1 AND criterion_id = ?2",
        params![
            commission_id,
            criterion.id,
            CriterionStatus::Passed.as_str()
        ],
        |row| row.get::<_, bool>(0),
    )?;
    transaction.execute(
        "UPDATE verification_gates
         SET status = ?4, closed_at = ?5
         WHERE commission_id = ?1 AND criterion_id = ?2 AND mandate_revision = ?3",
        params![
            commission_id,
            criterion.id,
            mandate_revision,
            if criterion_passed { "closed" } else { "open" },
            criterion_passed.then(unix_timestamp).transpose()?,
        ],
    )?;
    Ok(())
}

fn resolve_verification_recoveries(
    transaction: &Transaction<'_>,
    commission_id: &str,
    criterion_id: &str,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE verification_recoveries
         SET status = ?3, resolved_at = ?4
         WHERE commission_id = ?1 AND criterion_id = ?2
           AND status IN (?5, ?6, ?7)",
        params![
            commission_id,
            criterion_id,
            VerificationRecoveryStatus::Resolved.as_str(),
            unix_timestamp()?,
            VerificationRecoveryStatus::Pending.as_str(),
            VerificationRecoveryStatus::Scheduled.as_str(),
            VerificationRecoveryStatus::AttentionRequired.as_str(),
        ],
    )?;
    Ok(())
}

fn resolve_all_verification_recoveries(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE verification_recoveries
         SET status = ?2, resolved_at = ?3
         WHERE commission_id = ?1 AND status IN (?4, ?5, ?6)",
        params![
            commission_id,
            VerificationRecoveryStatus::Resolved.as_str(),
            unix_timestamp()?,
            VerificationRecoveryStatus::Pending.as_str(),
            VerificationRecoveryStatus::Scheduled.as_str(),
            VerificationRecoveryStatus::AttentionRequired.as_str(),
        ],
    )?;
    Ok(())
}

fn plan_verification_recovery(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    evidence_id: &str,
    evidence: &VerificationEvidenceSubmission,
) -> Result<Option<String>, TyrionError> {
    let recovery = if evidence.material_contradiction {
        Some((
            VerificationRecoveryAction::Escalate,
            VerificationRecoveryStatus::AttentionRequired,
            "Resolve the material contradiction through Principal review or a verification amendment.",
        ))
    } else {
        match evidence.defect {
            Some(VerificationDefect::Result) => Some((
                VerificationRecoveryAction::Rework,
                VerificationRecoveryStatus::Pending,
                "Rework the current Result under the accepted mandate.",
            )),
            Some(VerificationDefect::Environment) => Some((
                VerificationRecoveryAction::Retry,
                VerificationRecoveryStatus::Pending,
                "Retry verification in the required current environment with a fresh Verification Attempt.",
            )),
            Some(VerificationDefect::Verifier) => Some((
                VerificationRecoveryAction::Reroute,
                VerificationRecoveryStatus::Pending,
                "Reroute verification to a distinct eligible verifier Attachment or configuration.",
            )),
            Some(VerificationDefect::Criterion) => Some((
                VerificationRecoveryAction::Escalate,
                VerificationRecoveryStatus::AttentionRequired,
                "Escalate for Principal clarification and a revision-checked verification amendment.",
            )),
            None => None,
        }
    };
    let Some((action, status, requirement)) = recovery else {
        return Ok(None);
    };
    let recovery_id = Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO verification_recoveries (
            id, commission_id, criterion_id, result_id, source_evidence_id,
            mandate_revision, action, status, requirement, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            recovery_id,
            commission_id,
            evidence.criterion_id,
            evidence.result_id,
            evidence_id,
            mandate_revision,
            action.as_str(),
            status.as_str(),
            requirement,
            unix_timestamp()?,
        ],
    )?;
    Ok(Some(recovery_id))
}

fn refresh_criterion_statuses(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    artifact_revision: &str,
) -> Result<(), TyrionError> {
    let mut criteria_statement = transaction.prepare(
        "SELECT criterion_id, required_evidence, verifier_type, verification_depth,
                verifier_configuration, verification_environment, verifier_kind, expected
         FROM criteria WHERE commission_id = ?1 ORDER BY position",
    )?;
    let criteria = criteria_statement
        .query_map([commission_id], stored_criterion)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(criteria_statement);

    for criterion in criteria {
        let mut evidence_statement = transaction.prepare(
            "SELECT outcome, verifier_identity, verification_attempt_id,
                    material_contradiction
             FROM evidence
             WHERE commission_id = ?1 AND criterion_id = ?2
               AND mandate_revision = ?3 AND artifact_revision = ?4
               AND evidence_type = ?5 AND verifier_type = ?6
               AND verifier_configuration = ?7 AND environment = ?8
               AND verifier_kind = ?9 AND expected = ?10
               AND scope IN ('integrated', 'external')
             ORDER BY rowid",
        )?;
        let records = evidence_statement
            .query_map(
                params![
                    commission_id,
                    criterion.id,
                    mandate_revision,
                    artifact_revision,
                    criterion.required_evidence,
                    criterion.verifier_type.as_str(),
                    criterion.verifier_configuration,
                    criterion.verification_environment,
                    criterion.verifier_kind,
                    criterion.expected,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        let has_material_contradiction = records.iter().any(|record| record.3);
        let deterministic_failure = criterion.verifier_type == VerifierType::Deterministic
            && records
                .iter()
                .any(|record| record.0 == VerificationVerdict::Failed.as_str());
        let mut latest_by_identity = HashMap::new();
        for record in &records {
            latest_by_identity.insert(record.1.as_str(), record);
        }
        let current_records = latest_by_identity.values().copied().collect::<Vec<_>>();
        let has_failure = deterministic_failure
            || current_records
                .iter()
                .any(|record| record.0 == VerificationVerdict::Failed.as_str());
        let passed_attempts = current_records
            .iter()
            .filter(|record| record.0 == VerificationVerdict::Passed.as_str())
            .map(|record| record.2.as_str())
            .collect::<HashSet<_>>();
        let required_passes = criterion.verification_depth.required_passes();
        let status = if has_material_contradiction {
            CriterionStatus::Uncertain
        } else if passed_attempts.len() >= required_passes {
            CriterionStatus::Passed
        } else if has_failure {
            CriterionStatus::Failed
        } else {
            CriterionStatus::Uncertain
        };
        transaction.execute(
            "UPDATE criteria SET status = ?3 WHERE commission_id = ?1 AND criterion_id = ?2",
            params![commission_id, criterion.id, status.as_str()],
        )?;
    }
    Ok(())
}

fn complete_after_external_verification(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    artifact_revision: &str,
) -> Result<(), TyrionError> {
    let unresolved = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM criteria WHERE commission_id = ?1 AND status != ?2
         )",
        params![commission_id, CriterionStatus::Passed.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    let contradiction = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM evidence
            WHERE commission_id = ?1 AND mandate_revision = ?2
              AND artifact_revision = ?3 AND scope IN ('integrated', 'external')
              AND material_contradiction = 1
         )",
        params![commission_id, mandate_revision, artifact_revision],
        |row| row.get::<_, bool>(0),
    )?;
    if unresolved || contradiction {
        return Ok(());
    }

    let (result_id, assignment_id, goal) = transaction
        .query_row(
            "SELECT results.id, assignments.id, commissions.goal
             FROM results
             JOIN attempts ON attempts.id = results.attempt_id
             JOIN assignments ON assignments.id = attempts.assignment_id
             JOIN commissions ON commissions.id = assignments.commission_id
             WHERE assignments.commission_id = ?1
               AND results.integrated_artifact_revision = ?2
               AND results.mandate_revision = ?3
               AND results.status = ?4
             ORDER BY results.created_at DESC, results.id DESC
             LIMIT 1",
            params![
                commission_id,
                artifact_revision,
                mandate_revision,
                ResultStatus::Candidate.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "current Evidence does not identify one candidate integrated Result".into(),
            )
        })?;
    complete_commission(
        transaction,
        CompletionTransition {
            commission_id,
            result_id: &result_id,
            assignment_id: &assignment_id,
            attempt_id: None,
            lease_id: None,
            mandate_revision,
            artifact_revision,
            goal: &goal,
        },
    )
}

fn complete_commission(
    transaction: &Transaction<'_>,
    completion: CompletionTransition<'_>,
) -> Result<(), TyrionError> {
    let CompletionTransition {
        commission_id,
        result_id,
        assignment_id,
        attempt_id,
        lease_id,
        mandate_revision,
        artifact_revision,
        goal,
    } = completion;
    let preconditions_hold = transaction.query_row(
        "SELECT
            NOT EXISTS(
                SELECT 1 FROM criteria
                WHERE commission_id = ?1 AND status != ?4
            )
            AND NOT EXISTS(
                SELECT 1 FROM verification_gates
                WHERE commission_id = ?1 AND mandate_revision = ?2 AND status = 'open'
            )
            AND NOT EXISTS(
                SELECT 1 FROM approval_gates
                JOIN operation_requests
                  ON operation_requests.id = approval_gates.operation_request_id
                WHERE approval_gates.commission_id = ?1
                  AND operation_requests.mandate_revision = ?2
                  AND (
                      approval_gates.status != 'consumed'
                      OR operation_requests.status != 'confirmed'
                  )
            )
            AND NOT EXISTS(
                SELECT 1 FROM evidence
                JOIN results ON results.id = evidence.result_id
                WHERE evidence.commission_id = ?1
                  AND evidence.mandate_revision = ?2
                  AND evidence.artifact_revision = ?3
                  AND evidence.scope IN ('integrated', 'external')
                  AND evidence.material_contradiction = 1
                  AND results.status != ?5
            )",
        params![
            commission_id,
            mandate_revision,
            artifact_revision,
            CriterionStatus::Passed.as_str(),
            ResultStatus::Superseded.as_str(),
        ],
        |row| row.get::<_, bool>(0),
    )?;
    if !preconditions_hold {
        return Err(TyrionError::InvalidRequest(
            "Verified Completion requires passed current criteria, confirmed current Approval Gates, and no material contradiction"
                .into(),
        ));
    }

    let accepted_results = transaction.execute(
        "UPDATE results SET status = ?2 WHERE id = ?1 AND status = ?3",
        params![
            result_id,
            ResultStatus::Accepted.as_str(),
            ResultStatus::Candidate.as_str()
        ],
    )?;
    if accepted_results != 1 {
        return Err(TyrionError::InvalidRequest(
            "Verified Completion requires exactly one current candidate Result".into(),
        ));
    }
    record_result_profile_claim_outcome(transaction, result_id, ProfileClaimOutcome::Accepted)?;
    record_event(
        transaction,
        commission_id,
        EventKind::ResultAccepted,
        mandate_revision,
    )?;
    match (attempt_id, lease_id) {
        (Some(attempt_id), Some(lease_id)) => {
            release_successful_attempt(
                transaction,
                SuccessfulAttemptRelease {
                    attempt_id,
                    lease_id,
                },
            )?;
        }
        (None, None) => {}
        _ => {
            return Err(TyrionError::InvalidRequest(
                "completion Attempt and Worker Lease must be supplied together".into(),
            ));
        }
    }
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![assignment_id, AssignmentStatus::Accepted.as_str()],
    )?;
    finish_verified_commission(
        transaction,
        VerifiedCommissionCompletion {
            commission_id,
            mandate_revision,
            artifact_revision,
            goal,
        },
    )
}

fn accept_planned_result(
    transaction: &Transaction<'_>,
    commission_id: &str,
    acceptance: PlannedAcceptance<'_>,
) -> Result<(), TyrionError> {
    let accepted = transaction.execute(
        "UPDATE results SET status = ?2 WHERE id = ?1 AND status = ?3",
        params![
            acceptance.result_id,
            ResultStatus::Accepted.as_str(),
            ResultStatus::Candidate.as_str(),
        ],
    )?;
    if accepted != 1 {
        return Err(TyrionError::InvalidRequest(
            "planned Result acceptance requires one current candidate Result".into(),
        ));
    }
    release_successful_attempt(
        transaction,
        SuccessfulAttemptRelease {
            attempt_id: acceptance.attempt_id,
            lease_id: acceptance.lease_id,
        },
    )?;
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![
            acceptance.assignment_id,
            AssignmentStatus::Accepted.as_str()
        ],
    )?;
    record_event_with_payload(
        transaction,
        commission_id,
        EventKind::ResultAccepted,
        acceptance.mandate_revision,
        &serde_json::json!({
            "assignment_id": acceptance.assignment_id,
            "attempt_id": acceptance.attempt_id,
            "result_id": acceptance.result_id,
        }),
    )?;
    Ok(())
}

fn supersede_competing_candidates(
    transaction: &Transaction<'_>,
    commission_id: &str,
    competition_group: &str,
    reconciliation_assignment_id: &str,
    reconciliation_result_id: &str,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE results SET status = ?4, revision_disposition = 'superseded'
         WHERE id != ?3 AND status = ?5
           AND attempt_id IN (
               SELECT attempts.id FROM attempts
               JOIN assignments ON assignments.id = attempts.assignment_id
               JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
               WHERE assignments.commission_id = ?1
                 AND assignment_metadata.competition_group = ?2
                 AND assignment_metadata.purpose != 'reconciliation'
           )",
        params![
            commission_id,
            competition_group,
            reconciliation_result_id,
            ResultStatus::Superseded.as_str(),
            ResultStatus::Candidate.as_str(),
        ],
    )?;
    transaction.execute(
        "UPDATE attempts SET revision_disposition = 'superseded'
         WHERE id IN (
             SELECT results.attempt_id FROM results
             JOIN attempts ON attempts.id = results.attempt_id
             JOIN assignments ON assignments.id = attempts.assignment_id
             JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
             WHERE assignments.commission_id = ?1
               AND assignment_metadata.competition_group = ?2
               AND assignment_metadata.purpose != 'reconciliation'
               AND results.id != ?3
         )",
        params![commission_id, competition_group, reconciliation_result_id],
    )?;
    transaction.execute(
        "UPDATE assignments SET status = ?4
         WHERE commission_id = ?1 AND id != ?3 AND status IN (?5, ?6)
           AND id IN (
               SELECT assignment_id FROM assignment_metadata
               WHERE competition_group = ?2 AND purpose != 'reconciliation'
           )",
        params![
            commission_id,
            competition_group,
            reconciliation_assignment_id,
            AssignmentStatus::Superseded.as_str(),
            AssignmentStatus::VerificationPending.as_str(),
            AssignmentStatus::VerificationFailed.as_str(),
        ],
    )?;
    Ok(())
}

fn record_useful_concurrency(
    transaction: &Transaction<'_>,
    commission_id: &str,
    completed_attempt_id: &str,
    mandate_revision: i64,
) -> Result<(), TyrionError> {
    let mut intervals = {
        let mut statement = transaction.prepare(
            "SELECT attempts.id, attempts.started_at_ms,
                    attempts.execution_completed_at_ms
             FROM attempts
             JOIN assignments ON assignments.id = attempts.assignment_id
             JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
             LEFT JOIN planned_assignments
               ON planned_assignments.commission_id = assignments.commission_id
              AND planned_assignments.logical_id = assignment_metadata.logical_id
             JOIN results ON results.attempt_id = attempts.id
             WHERE assignments.commission_id = ?1
               AND attempts.status = ?2
               AND (
                   results.status = ?3
                   OR (
                       results.status = ?4
                       AND assignment_metadata.competition_group IS NOT NULL
                       AND assignment_metadata.purpose != 'reconciliation'
                       AND planned_assignments.competition_group = assignment_metadata.competition_group
                   )
               )
               AND attempts.execution_completed_at_ms IS NOT NULL
             ORDER BY attempts.started_at_ms, attempts.id",
        )?;
        let rows = statement.query_map(
            params![
                commission_id,
                AttemptStatus::Succeeded.as_str(),
                ResultStatus::Accepted.as_str(),
                ResultStatus::Superseded.as_str(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if intervals.len() < 2 {
        return Ok(());
    }
    intervals.sort_by_key(|(_, started_at_ms, _)| *started_at_ms);
    let first_started_at_ms = intervals[0].1;
    let serial_execution_millis = intervals.iter().fold(0_i64, |total, (_, start, end)| {
        total.saturating_add(end.saturating_sub(*start))
    });
    let mut union_execution_millis = 0_i64;
    let mut current_start = intervals[0].1;
    let mut current_end = intervals[0].2;
    for (_, start, end) in intervals.iter().skip(1) {
        if *start <= current_end {
            current_end = current_end.max(*end);
        } else {
            union_execution_millis =
                union_execution_millis.saturating_add(current_end.saturating_sub(current_start));
            current_start = *start;
            current_end = *end;
        }
    }
    union_execution_millis =
        union_execution_millis.saturating_add(current_end.saturating_sub(current_start));
    let elapsed_time_reduction_millis =
        serial_execution_millis.saturating_sub(union_execution_millis);
    if elapsed_time_reduction_millis > 0 {
        let attempt_ids = intervals
            .iter()
            .map(|(attempt_id, _, _)| attempt_id)
            .collect::<Vec<_>>();
        record_event_with_payload(
            transaction,
            commission_id,
            EventKind::UsefulConcurrencyObserved,
            mandate_revision,
            &serde_json::json!({
                "attempt_ids": attempt_ids,
                "trigger_attempt_id": completed_attempt_id,
                "overlap_millis": elapsed_time_reduction_millis,
                "serial_execution_millis": serial_execution_millis,
                "parallel_execution_window_millis": union_execution_millis,
                "elapsed_time_reduction_millis": elapsed_time_reduction_millis,
                "end_to_end_elapsed_millis": unix_timestamp_millis()?.saturating_sub(first_started_at_ms),
                "success_metric": "verified execution elapsed-time reduction",
            }),
        )?;
    }
    Ok(())
}

fn advance_plan_after_evidence(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    result_id: &str,
) -> Result<(), TyrionError> {
    let (prior_revision, prior_snapshot) = transaction.query_row(
        "SELECT revision, snapshot_json FROM commission_plans
         WHERE commission_id = ?1 ORDER BY revision DESC LIMIT 1",
        [commission_id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )?;
    let next_revision = prior_revision + 1;
    let ready_logical_ids = {
        let mut statement = transaction.prepare(
            "SELECT planned_assignments.logical_id
             FROM planned_assignments
             WHERE planned_assignments.commission_id = ?1
               AND planned_assignments.purpose != 'reconciliation'
               AND NOT EXISTS(
                   SELECT 1 FROM assignment_metadata
                   JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                   WHERE assignments.commission_id = planned_assignments.commission_id
                     AND assignment_metadata.logical_id = planned_assignments.logical_id
               )
               AND NOT EXISTS(
                   SELECT 1 FROM planned_assignment_dependencies
                   WHERE planned_assignment_dependencies.commission_id = planned_assignments.commission_id
                     AND planned_assignment_dependencies.assignment_logical_id = planned_assignments.logical_id
                     AND NOT EXISTS(
                         SELECT 1 FROM assignment_metadata
                         JOIN assignments ON assignments.id = assignment_metadata.assignment_id
                         WHERE assignments.commission_id = planned_assignments.commission_id
                           AND assignment_metadata.logical_id = planned_assignment_dependencies.dependency_logical_id
                           AND assignments.status IN (?2, ?3)
                     )
               )
             ORDER BY planned_assignments.position",
        )?;
        let rows = statement.query_map(
            params![
                commission_id,
                AssignmentStatus::Accepted.as_str(),
                AssignmentStatus::Superseded.as_str()
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let now = unix_timestamp()?;
    for logical_id in &ready_logical_ids {
        insert_ready_assignment(
            transaction,
            commission_id,
            logical_id,
            next_revision,
            mandate_revision,
            false,
            now,
        )?;
    }
    let execution_frontier = execution_frontier_logical_ids(transaction, commission_id)?;
    let mut snapshot = serde_json::from_str::<Value>(&prior_snapshot)?;
    snapshot["evidence_result_id"] = Value::String(result_id.to_owned());
    snapshot["execution_frontier"] = serde_json::to_value(&execution_frontier)?;
    transaction.execute(
        "INSERT INTO commission_plans (
            commission_id, revision, source, reason, snapshot_json, created_at
         ) VALUES (?1, ?2, 'control_plane', ?3, ?4, ?5)",
        params![
            commission_id,
            next_revision,
            "current Evidence advanced the safe Execution Frontier",
            serde_json::to_string(&snapshot)?,
            now,
        ],
    )?;
    record_event_with_payload(
        transaction,
        commission_id,
        EventKind::PlanRevised,
        mandate_revision,
        &serde_json::json!({
            "plan_revision": next_revision,
            "reason": "evidence_advanced_frontier",
            "source_result_id": result_id,
            "execution_frontier": execution_frontier,
        }),
    )?;
    Ok(())
}

fn execution_frontier_logical_ids(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<Vec<String>, TyrionError> {
    Ok(
        project_commission(transaction, commission_id)?["execution_frontier"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|assignment| assignment["logical_id"].as_str())
            .map(str::to_owned)
            .collect(),
    )
}

fn complete_planned_commission(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    artifact_revision: &str,
) -> Result<(), TyrionError> {
    let goal = transaction.query_row(
        "SELECT goal FROM commissions WHERE id = ?1",
        [commission_id],
        |row| row.get::<_, String>(0),
    )?;
    finish_verified_commission(
        transaction,
        VerifiedCommissionCompletion {
            commission_id,
            mandate_revision,
            artifact_revision,
            goal: &goal,
        },
    )
}

fn finish_verified_commission(
    transaction: &Transaction<'_>,
    completion: VerifiedCommissionCompletion<'_>,
) -> Result<(), TyrionError> {
    let completed_at = unix_timestamp()?;
    let completion_revision = completion.mandate_revision + 1;
    let updated = transaction.execute(
        "UPDATE commissions
         SET status = ?2, revision = ?3, completed_at = ?4, artifact_revision = ?5
         WHERE id = ?1 AND status IN (?6, ?7) AND revision = ?8",
        params![
            completion.commission_id,
            CommissionStatus::VerifiedComplete.as_str(),
            completion_revision,
            completed_at,
            completion.artifact_revision,
            CommissionStatus::Active.as_str(),
            CommissionStatus::Paused.as_str(),
            completion.mandate_revision,
        ],
    )?;
    if updated != 1 {
        return Err(TyrionError::StaleRevision {
            expected: completion.mandate_revision,
            actual: transaction.query_row(
                "SELECT revision FROM commissions WHERE id = ?1",
                [completion.commission_id],
                |row| row.get(0),
            )?,
        });
    }
    transaction.execute(
        "INSERT INTO completion_briefings (commission_id, summary, artifact_revision, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            completion.commission_id,
            format!("Verified Complete: {}", completion.goal),
            completion.artifact_revision,
            completed_at,
        ],
    )?;
    finalize_temporary_material_retention(transaction, completion.commission_id, completed_at)?;
    record_event(
        transaction,
        completion.commission_id,
        EventKind::CommissionVerifiedComplete,
        completion_revision,
    )?;
    Ok(())
}

fn finalize_temporary_material_retention(
    transaction: &Transaction<'_>,
    commission_id: &str,
    completed_at: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "DELETE FROM temporary_memory_materials
         WHERE commission_id = ?1 AND kind = 'unaccepted_artifact'
           AND result_id IN (SELECT id FROM results WHERE status = 'accepted')",
        [commission_id],
    )?;
    refresh_temporary_material_retention_links(transaction, Some(commission_id))?;
    let expires_at = completed_at.saturating_add(30 * 24 * 60 * 60);
    transaction.execute(
        "UPDATE temporary_memory_materials
         SET expires_at = ?2
         WHERE commission_id = ?1 AND expired_at IS NULL",
        params![commission_id, expires_at],
    )?;
    Ok(())
}

fn refresh_temporary_material_retention_links(
    transaction: &Transaction<'_>,
    commission_id: Option<&str>,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE temporary_memory_materials
         SET retained_by_evidence = kind = 'unaccepted_artifact' AND EXISTS (
                 SELECT 1 FROM evidence
                 WHERE evidence.result_id = temporary_memory_materials.result_id
             ),
             retained_by_claim = EXISTS (
                 SELECT 1 FROM learning_observations
                 JOIN profile_claim_observations
                   ON profile_claim_observations.observation_id = learning_observations.id
                 WHERE learning_observations.commission_id =
                       temporary_memory_materials.commission_id
             ),
             retained_for_uncertain_effect = EXISTS (
                 SELECT 1 FROM operation_requests
                 WHERE operation_requests.commission_id =
                       temporary_memory_materials.commission_id
                   AND operation_requests.status = 'uncertain'
             )
         WHERE (?1 IS NULL OR commission_id = ?1) AND expired_at IS NULL",
        [commission_id],
    )?;
    Ok(())
}

fn route_result_rework(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    evidence: &VerificationEvidenceSubmission,
    recovery_id: Option<&str>,
) -> Result<(), TyrionError> {
    if evidence.verdict != VerificationVerdict::Failed
        || evidence.defect != Some(VerificationDefect::Result)
        || evidence.material_contradiction
    {
        return Ok(());
    }
    let (assignment_id, assignment_status, attempt_count, max_attempts) = transaction.query_row(
        "SELECT assignments.id, assignments.status,
                (SELECT COUNT(*) FROM attempts AS counted
                 WHERE counted.assignment_id = assignments.id
                   AND NOT EXISTS (
                       SELECT 1 FROM worker_configuration_failures
                       WHERE worker_configuration_failures.attempt_id = counted.id
                   )),
                resource_ceilings.max_attempts
         FROM results
         JOIN attempts ON attempts.id = results.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         JOIN resource_ceilings ON resource_ceilings.commission_id = assignments.commission_id
         WHERE results.id = ?1 AND assignments.commission_id = ?2",
        params![evidence.result_id, commission_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, u32>(3)?,
            ))
        },
    )?;
    if assignment_status != AssignmentStatus::VerificationPending.as_str() {
        return Ok(());
    }
    if attempt_count < max_attempts {
        transaction.execute(
            "UPDATE results SET status = ?2 WHERE id = ?1",
            params![evidence.result_id, ResultStatus::Superseded.as_str()],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![assignment_id, AssignmentStatus::Ready.as_str()],
        )?;
        transaction.execute(
            "UPDATE criteria SET status = ?2 WHERE commission_id = ?1",
            params![commission_id, CriterionStatus::Uncertain.as_str()],
        )?;
        record_event(
            transaction,
            commission_id,
            EventKind::AssignmentReady,
            mandate_revision,
        )?;
        if let Some(recovery_id) = recovery_id {
            transaction.execute(
                "UPDATE verification_recoveries SET status = ?2 WHERE id = ?1",
                params![recovery_id, VerificationRecoveryStatus::Scheduled.as_str()],
            )?;
        }
    } else {
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![assignment_id, AssignmentStatus::ResourceBlocked.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO blockers (
                id, commission_id, assignment_id, code, requirement, created_at
             ) VALUES (?1, ?2, ?3, 'max_attempts', ?4, ?5)",
            params![
                Uuid::new_v4().to_string(),
                commission_id,
                assignment_id,
                "Start a new Commission with a higher max_attempts ceiling to rework the failed Result.",
                unix_timestamp()?,
            ],
        )?;
        record_event(
            transaction,
            commission_id,
            EventKind::AssignmentBlocked,
            mandate_revision,
        )?;
        if let Some(recovery_id) = recovery_id {
            transaction.execute(
                "UPDATE verification_recoveries
                 SET action = ?2, status = ?3, requirement = ?4
                 WHERE id = ?1",
                params![
                    recovery_id,
                    VerificationRecoveryAction::Block.as_str(),
                    VerificationRecoveryStatus::Blocked.as_str(),
                    "No authorized rework Attempt remains under max_attempts.",
                ],
            )?;
        }
    }
    Ok(())
}

fn validate_evidence_submission(
    evidence: &VerificationEvidenceSubmission,
) -> Result<(), TyrionError> {
    if evidence.criterion_id.trim().is_empty()
        || evidence.result_id.trim().is_empty()
        || evidence.evidence_type.trim().is_empty()
        || evidence.verifier_configuration.trim().is_empty()
        || evidence.environment.trim().is_empty()
        || evidence.inspectable_output.trim().is_empty()
    {
        return Err(TyrionError::InvalidRequest(
            "verification Evidence identifiers, bindings, and inspectable output must not be empty"
                .into(),
        ));
    }
    match (evidence.verdict, evidence.defect) {
        (VerificationVerdict::Passed, None)
        | (VerificationVerdict::Failed | VerificationVerdict::Uncertain, Some(_)) => Ok(()),
        (VerificationVerdict::Passed, Some(_)) => Err(TyrionError::InvalidRequest(
            "passed Evidence must not diagnose a defect".into(),
        )),
        (VerificationVerdict::Failed | VerificationVerdict::Uncertain, None) => {
            Err(TyrionError::InvalidRequest(
                "failed or uncertain Evidence must diagnose a Result, verifier, environment, or criterion defect".into(),
            ))
        }
    }
}

fn verifier_storage(verifier: &Verifier) -> Result<(&'static str, String), TyrionError> {
    match verifier {
        Verifier::ExactMatch { expected } => Ok(("exact_match", expected.clone())),
        Verifier::Command { argv } => Ok(("command", serde_json::to_string(argv)?)),
        Verifier::Prompt { prompt } => Ok(("prompt", prompt.clone())),
    }
}

fn stored_criterion(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCriterion> {
    let verifier_type = stored_verifier_type(row.get::<_, String>(2)?, 2)?;
    let verification_depth = stored_verification_depth(row.get::<_, String>(3)?, 3)?;
    Ok(StoredCriterion {
        id: row.get(0)?,
        required_evidence: row.get(1)?,
        verifier_type,
        verification_depth,
        verifier_configuration: row.get(4)?,
        verification_environment: row.get(5)?,
        verifier_kind: row.get(6)?,
        expected: row.get(7)?,
    })
}

fn stored_verifier_type(value: String, index: usize) -> rusqlite::Result<VerifierType> {
    match value.as_str() {
        "deterministic" => Ok(VerifierType::Deterministic),
        "model" => Ok(VerifierType::Model),
        "principal" => Ok(VerifierType::Principal),
        _ => Err(invalid_stored_enum(index, "verifier type", value)),
    }
}

fn stored_verification_depth(value: String, index: usize) -> rusqlite::Result<VerificationDepth> {
    match value.as_str() {
        "standard" => Ok(VerificationDepth::Standard),
        "independent" => Ok(VerificationDepth::Independent),
        _ => Err(invalid_stored_enum(index, "verification depth", value)),
    }
}

fn invalid_stored_enum(index: usize, field: &str, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid stored {field}: {value}"),
        )),
    )
}

fn finish_verification(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    attempt_id: &str,
    lease_id: &str,
    assignment_status: AssignmentStatus,
) -> Result<(), TyrionError> {
    release_successful_attempt(
        transaction,
        SuccessfulAttemptRelease {
            attempt_id,
            lease_id,
        },
    )?;
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![assignment_id, assignment_status.as_str()],
    )?;
    Ok(())
}

fn attempt_continuation(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ready: &ReadyAssignmentDispatch,
    attempt_id: &str,
) -> Result<AttemptContinuation, TyrionError> {
    let execution: ExecutionSpec = serde_json::from_str(&ready.execution_json)?;
    let (
        commission_status,
        commission_revision,
        assignment_status,
        assignment_plan_revision,
        attempt_status,
        cleanup_pending,
    ) = transaction.query_row(
        "SELECT commissions.status, commissions.revision,
                assignments.status, assignments.plan_revision, attempts.status,
                EXISTS(
                    SELECT 1 FROM sandbox_cleanups WHERE attempt_id = attempts.id
                )
         FROM commissions
         JOIN assignments ON assignments.commission_id = commissions.id
         JOIN attempts ON attempts.assignment_id = assignments.id
         WHERE commissions.id = ?1 AND assignments.id = ?2 AND attempts.id = ?3",
        params![commission_id, ready.assignment_id, attempt_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, bool>(5)?,
            ))
        },
    )?;
    if commission_status == CommissionStatus::Cancelled.as_str()
        || assignment_status == AssignmentStatus::Cancelled.as_str()
        || attempt_status == AttemptStatus::Cancelled.as_str()
    {
        return Ok(AttemptContinuation::Cancelled);
    }
    let commission_dispatchable = commission_status == CommissionStatus::Active.as_str()
        || commission_status == CommissionStatus::Paused.as_str();
    let authority_valid =
        attempt_authority_is_current(transaction, commission_id, attempt_id, &execution)?;
    if commission_dispatchable
        && commission_revision == ready.mandate_revision
        && assignment_status == AssignmentStatus::Running.as_str()
        && assignment_plan_revision == ready.plan_revision
        && attempt_status == AttemptStatus::Running.as_str()
        && !cleanup_pending
        && authority_valid
    {
        Ok(AttemptContinuation::Current)
    } else {
        Ok(AttemptContinuation::Stale {
            commission_revision,
            attempt_running: attempt_status == AttemptStatus::Running.as_str() && !cleanup_pending,
        })
    }
}

fn retain_noncurrent_result(
    transaction: &Transaction<'_>,
    continuation: AttemptContinuation,
    attempt_id: &str,
    lease_id: &str,
    result_id: &str,
    integrated: bool,
) -> Result<(), TyrionError> {
    match continuation {
        AttemptContinuation::Current => {}
        AttemptContinuation::Cancelled => {
            transaction.execute(
                "UPDATE results
                 SET status = 'superseded', revision_disposition = 'retained'
                 WHERE id = ?1",
                [result_id],
            )?;
        }
        AttemptContinuation::Stale {
            attempt_running, ..
        } => {
            let disposition = if integrated {
                "requires_revalidation"
            } else {
                "stale"
            };
            transaction.execute(
                "UPDATE results
                 SET status = 'superseded', revision_disposition = ?2
                 WHERE id = ?1",
                params![result_id, disposition],
            )?;
            if attempt_running {
                release_successful_attempt(
                    transaction,
                    SuccessfulAttemptRelease {
                        attempt_id,
                        lease_id,
                    },
                )?;
            }
            transaction.execute(
                "UPDATE attempts SET revision_disposition = ?2 WHERE id = ?1",
                params![attempt_id, disposition],
            )?;
        }
    }
    Ok(())
}

fn recover_failed_verification(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ready: &ReadyAssignmentDispatch,
    attempt_id: &str,
    lease_id: &str,
    result_id: &str,
) -> Result<(), TyrionError> {
    release_successful_attempt(
        transaction,
        SuccessfulAttemptRelease {
            attempt_id,
            lease_id,
        },
    )?;
    transaction.execute(
        "UPDATE attempts SET revision_disposition = 'retained' WHERE id = ?1",
        [attempt_id],
    )?;
    transaction.execute(
        "UPDATE results SET status = 'superseded', revision_disposition = 'superseded'
         WHERE id = ?1",
        [result_id],
    )?;
    let equivalence_key = format!("verification_failure:{}", ready.logical_id);
    let equivalent_failures = transaction.query_row(
        "SELECT COUNT(*) FROM attempt_recoveries
         WHERE commission_id = ?1 AND equivalence_key = ?2",
        params![commission_id, equivalence_key],
        |row| row.get::<_, u32>(0),
    )?;
    let (attempt_count, max_attempts) = transaction.query_row(
        "SELECT (
            SELECT COUNT(*) FROM attempts
            JOIN assignments ON assignments.id = attempts.assignment_id
            WHERE assignments.commission_id = ?1
         ), max_attempts
         FROM resource_ceilings WHERE commission_id = ?1",
        [commission_id],
        |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
    )?;
    if attempt_count >= max_attempts {
        let requirement =
            "Start a linked Commission with a higher max_attempts ceiling and the retained failed Evidence.";
        transaction.execute(
            "UPDATE results SET status = 'candidate', revision_disposition = 'retained'
             WHERE id = ?1",
            [result_id],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![
                ready.assignment_id,
                AssignmentStatus::VerificationFailed.as_str()
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO blockers (
                id, commission_id, assignment_id, code, requirement, created_at
             ) VALUES (?1, ?2, ?3, 'max_attempts', ?4, ?5)",
            params![
                Uuid::new_v4().to_string(),
                commission_id,
                ready.assignment_id,
                requirement,
                unix_timestamp()?,
            ],
        )?;
        record_attempt_recovery(
            transaction,
            AttemptRecovery {
                commission_id,
                assignment_id: &ready.assignment_id,
                attempt_id,
                cause: "verification_failure",
                classification: "repairable_context",
                equivalence_key: &equivalence_key,
                action: "block",
                requirement,
            },
        )?;
        record_event(
            transaction,
            commission_id,
            EventKind::AssignmentBlocked,
            ready.mandate_revision,
        )?;
        return Ok(());
    }
    if equivalent_failures == 0 {
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![ready.assignment_id, AssignmentStatus::Ready.as_str()],
        )?;
        transaction.execute(
            "UPDATE criteria SET status = 'uncertain'
             WHERE commission_id = ?1 AND criterion_id IN (
                 SELECT criterion_id FROM planned_assignment_criteria
                 WHERE commission_id = ?1 AND assignment_logical_id = ?2
             )",
            params![commission_id, ready.logical_id],
        )?;
        record_attempt_recovery(
            transaction,
            AttemptRecovery {
                commission_id,
                assignment_id: &ready.assignment_id,
                attempt_id,
                cause: "verification_failure",
                classification: "repairable_context",
                equivalence_key: &equivalence_key,
                action: "retry",
                requirement: "Retry the same Worker Configuration once with the retained failed Evidence as repair context.",
            },
        )?;
        record_event_with_payload(
            transaction,
            commission_id,
            EventKind::AssignmentReady,
            ready.mandate_revision,
            &serde_json::json!({
                "assignment_id": ready.assignment_id,
                "reason": "bounded_same_configuration_retry",
                "prior_attempt_id": attempt_id,
            }),
        )?;
        return Ok(());
    }

    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![ready.assignment_id, AssignmentStatus::Superseded.as_str()],
    )?;
    let next_plan_revision = transaction.query_row(
        "SELECT COALESCE(MAX(revision), 0) + 1 FROM commission_plans WHERE commission_id = ?1",
        [commission_id],
        |row| row.get::<_, i64>(0),
    )?;
    let recovery_logical_id = format!("{}-recovery-{next_plan_revision}", ready.logical_id);
    let mut requirements: WorkerRequirements = transaction.query_row(
        "SELECT worker_requirements_json FROM planned_assignments
         WHERE commission_id = ?1 AND logical_id = ?2",
        params![commission_id, ready.logical_id],
        |row| {
            let encoded = row.get::<_, String>(0)?;
            serde_json::from_str(&encoded).map_err(|error| invalid_json_column(0, error))
        },
    )?;
    let failed_configuration = transaction.query_row(
        "SELECT worker_configuration FROM attempts WHERE id = ?1",
        [attempt_id],
        |row| row.get::<_, String>(0),
    )?;
    if !requirements
        .exclude_configurations
        .contains(&failed_configuration)
    {
        requirements
            .exclude_configurations
            .push(failed_configuration.clone());
    }
    transaction.execute(
        "INSERT INTO planned_assignments (
            commission_id, logical_id, position, goal, purpose,
            read_scopes_json, write_scopes_json, concurrency_slots,
            max_storage_bytes, max_model_spend_cents, max_paid_service_spend_cents,
            worker_requirements_json, competition_group, competition_uncertainty,
            competition_rule, created_plan_revision
         )
         SELECT commission_id, ?3, position, goal, purpose,
                read_scopes_json, write_scopes_json, concurrency_slots,
                max_storage_bytes, max_model_spend_cents, max_paid_service_spend_cents,
                ?4, competition_group, competition_uncertainty, competition_rule, ?5
         FROM planned_assignments WHERE commission_id = ?1 AND logical_id = ?2",
        params![
            commission_id,
            ready.logical_id,
            recovery_logical_id,
            serde_json::to_string(&requirements)?,
            next_plan_revision,
        ],
    )?;
    transaction.execute(
        "INSERT INTO planned_assignment_dependencies (
            commission_id, assignment_logical_id, dependency_logical_id, position
         )
         SELECT commission_id, ?3, dependency_logical_id, position
         FROM planned_assignment_dependencies
         WHERE commission_id = ?1 AND assignment_logical_id = ?2",
        params![commission_id, ready.logical_id, recovery_logical_id],
    )?;
    transaction.execute(
        "INSERT INTO planned_assignment_criteria (
            commission_id, assignment_logical_id, criterion_id, position
         )
         SELECT commission_id, ?3, criterion_id, position
         FROM planned_assignment_criteria
         WHERE commission_id = ?1 AND assignment_logical_id = ?2",
        params![commission_id, ready.logical_id, recovery_logical_id],
    )?;
    transaction.execute(
        "UPDATE planned_assignment_dependencies
         SET dependency_logical_id = ?3
         WHERE commission_id = ?1 AND dependency_logical_id = ?2",
        params![commission_id, ready.logical_id, recovery_logical_id],
    )?;
    let replacement_assignment_id = insert_ready_assignment(
        transaction,
        commission_id,
        &recovery_logical_id,
        next_plan_revision,
        ready.mandate_revision,
        ready.legacy,
        unix_timestamp()?,
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO watchdog_findings (
            id, commission_id, assignment_id, attempt_id, signal, action, details, created_at
         ) VALUES (?1, ?2, ?3, ?4, 'repeated_verification_failure',
                   'contain_attempt', ?5, ?6)",
        params![
            Uuid::new_v4().to_string(),
            commission_id,
            ready.assignment_id,
            attempt_id,
            "A second equivalent verification failure requires a revised plan rather than another blind retry.",
            unix_timestamp()?,
        ],
    )?;
    let requirement = format!(
        "Provide an eligible Worker Configuration other than {failed_configuration}, or revise the Assignment decomposition before resuming."
    );
    let snapshot = serde_json::json!({
        "reason": "second_equivalent_failure",
        "superseded_assignment_id": ready.assignment_id,
        "replacement_assignment_id": replacement_assignment_id,
        "failed_configuration": failed_configuration,
        "requirement": requirement,
    });
    transaction.execute(
        "INSERT INTO commission_plans (
            commission_id, revision, source, reason, snapshot_json, created_at
         ) VALUES (?1, ?2, 'control_plane', 'second equivalent verification failure', ?3, ?4)",
        params![
            commission_id,
            next_plan_revision,
            serde_json::to_string(&snapshot)?,
            unix_timestamp()?,
        ],
    )?;
    record_attempt_recovery(
        transaction,
        AttemptRecovery {
            commission_id,
            assignment_id: &ready.assignment_id,
            attempt_id,
            cause: "verification_failure",
            classification: "repairable_context",
            equivalence_key: &equivalence_key,
            action: "replan",
            requirement: &requirement,
        },
    )?;
    record_event_with_payload(
        transaction,
        commission_id,
        EventKind::PlanRevised,
        ready.mandate_revision,
        &serde_json::json!({
            "plan_revision": next_plan_revision,
            "reason": "second_equivalent_failure",
            "superseded_assignment_id": ready.assignment_id,
            "replacement_assignment_id": replacement_assignment_id,
        }),
    )?;
    Ok(())
}

fn release_successful_attempt(
    transaction: &Transaction<'_>,
    release: SuccessfulAttemptRelease<'_>,
) -> Result<(), TyrionError> {
    let now = unix_timestamp()?;
    let now_ms = unix_timestamp_millis()?;
    transaction.execute(
        "UPDATE attempts
         SET status = ?2, completed_at = ?3, completed_at_ms = ?4 WHERE id = ?1",
        params![
            release.attempt_id,
            AttemptStatus::Succeeded.as_str(),
            now,
            now_ms
        ],
    )?;
    transaction.execute(
        "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
        params![release.lease_id, WorkerLeaseStatus::Released.as_str(), now],
    )?;
    transaction.execute(
        "UPDATE resource_reservations
         SET status = ?2, released_at = ?3 WHERE attempt_id = ?1",
        params![release.attempt_id, "released", now_ms],
    )?;
    transaction.execute(
        "UPDATE workers
         SET status = 'succeeded', latest_activity = 'Result accepted', activity_at_ms = ?2
         WHERE attempt_id = ?1",
        params![release.attempt_id, now_ms],
    )?;
    Ok(())
}

fn block_ready_assignment(
    transaction: Transaction<'_>,
    commission_id: &str,
    assignment_id: &str,
    mandate_revision: i64,
    code: &str,
    requirement: &str,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![assignment_id, AssignmentStatus::ResourceBlocked.as_str()],
    )?;
    transaction.execute(
        "INSERT INTO blockers (id, commission_id, assignment_id, code, requirement, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            Uuid::new_v4().to_string(),
            commission_id,
            assignment_id,
            code,
            requirement,
            unix_timestamp()?
        ],
    )?;
    record_event(
        &transaction,
        commission_id,
        EventKind::AssignmentBlocked,
        mandate_revision,
    )?;
    transaction.commit()?;
    Ok(())
}

fn record_result_skill_outcomes(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ready: &ReadyAssignmentDispatch,
    attempt_id: &str,
    result_id: &str,
    verification_outcome: &str,
    usage: &Value,
) -> Result<(), TyrionError> {
    let skill_defaults = load_assignment_skill_defaults(transaction, &ready.assignment_id)?;
    if skill_defaults.is_empty() {
        return Ok(());
    }
    let (worker_configuration, configuration_json, started_at_ms, completed_at_ms) = transaction
        .query_row(
            "SELECT attempts.worker_configuration, workers.configuration_json,
                    attempts.started_at_ms,
                    COALESCE(attempts.execution_completed_at_ms, attempts.completed_at_ms,
                             attempts.started_at_ms)
             FROM attempts
             JOIN workers ON workers.attempt_id = attempts.id
             WHERE attempts.id = ?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
    let configuration: Value = serde_json::from_str(&configuration_json)?;
    let harness = configuration["harness"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let corrections = transaction.query_row(
        "SELECT COUNT(*) FROM attempt_recoveries WHERE assignment_id = ?1",
        [&ready.assignment_id],
        |row| row.get::<_, u64>(0),
    )?;
    let principal_intervention = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM worker_commands
            JOIN workers ON workers.id = worker_commands.worker_id
            WHERE workers.attempt_id = ?1
         )",
        [attempt_id],
        |row| row.get::<_, bool>(0),
    )?;
    let evidence_ids = {
        let mut statement = transaction
            .prepare("SELECT id FROM evidence WHERE result_id = ?1 ORDER BY created_at, rowid")?;
        let rows = statement.query_map([result_id], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let cost_cents = usage["cost_cents"].as_u64().unwrap_or(0);
    let latency_ms = completed_at_ms.saturating_sub(started_at_ms).max(0) as u64;
    let observed_at = unix_timestamp()?;
    let observation = if verification_outcome == "passed" {
        "verified_success"
    } else {
        "verification_failure"
    };
    let confidence_basis_points = if verification_outcome == "passed" {
        8000_u16
    } else {
        6500_u16
    };
    for skill in skill_defaults {
        transaction.execute(
            "INSERT OR IGNORE INTO result_skill_executions (
                result_id, skill_name, content_digest, requirement, provenance,
                worker_configuration, assignment_class, verification_outcome,
                corrections, cost_cents, latency_ms, principal_intervention, delegation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13)",
            params![
                result_id,
                skill.name,
                skill.content_digest,
                skill.requirement.as_str(),
                skill.provenance.as_str(),
                worker_configuration,
                ready.purpose,
                verification_outcome,
                corrections,
                cost_cents,
                latency_ms,
                principal_intervention,
                skill.delegation.as_str(),
            ],
        )?;
        let scope = serde_json::json!({
            "commission_id": commission_id,
            "assignment_id": ready.assignment_id,
            "worker_configuration": worker_configuration,
            "harness": harness,
            "assignment_class": ready.purpose,
        });
        transaction.execute(
            "INSERT OR IGNORE INTO skill_associations (
                id, commission_id, assignment_id, attempt_id, result_id,
                skill_name, content_digest, worker_configuration, harness,
                assignment_class, observation, verification_outcome, corrections,
                cost_cents, latency_ms, principal_intervention, evidence_ids_json,
                scope_json, confidence_basis_points, observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                Uuid::new_v4().to_string(),
                commission_id,
                ready.assignment_id,
                attempt_id,
                result_id,
                skill.name,
                skill.content_digest,
                worker_configuration,
                harness,
                ready.purpose,
                observation,
                verification_outcome,
                corrections,
                cost_cents,
                latency_ms,
                principal_intervention,
                serde_json::to_string(&evidence_ids)?,
                serde_json::to_string(&scope)?,
                confidence_basis_points,
                observed_at,
            ],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_required_skill_failure_association(
    transaction: &Transaction<'_>,
    commission_id: &str,
    assignment_id: &str,
    attempt_id: &str,
    worker_configuration: &str,
    skill_name: &str,
    content_digest: &str,
    message: &str,
    observed_at_ms: i64,
) -> Result<(), TyrionError> {
    let (configuration_json, assignment_class, started_at_ms) = transaction.query_row(
        "SELECT workers.configuration_json, assignment_metadata.purpose,
                attempts.started_at_ms
         FROM workers
         JOIN attempts ON attempts.id = workers.attempt_id
         JOIN assignment_metadata ON assignment_metadata.assignment_id = attempts.assignment_id
         WHERE attempts.id = ?1",
        [attempt_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let configuration: Value = serde_json::from_str(&configuration_json)?;
    let harness = configuration["harness"].as_str().unwrap_or("unknown");
    let corrections = transaction.query_row(
        "SELECT COUNT(*) FROM attempt_recoveries WHERE assignment_id = ?1",
        [assignment_id],
        |row| row.get::<_, u64>(0),
    )?;
    let principal_intervention = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM worker_commands
            JOIN workers ON workers.id = worker_commands.worker_id
            WHERE workers.attempt_id = ?1
         )",
        [attempt_id],
        |row| row.get::<_, bool>(0),
    )?;
    let evidence = serde_json::json!([{
        "kind": "harness_report",
        "attempt_id": attempt_id,
        "message": message,
    }]);
    let scope = serde_json::json!({
        "commission_id": commission_id,
        "assignment_id": assignment_id,
        "worker_configuration": worker_configuration,
        "harness": harness,
        "assignment_class": assignment_class,
    });
    transaction.execute(
        "INSERT OR IGNORE INTO skill_associations (
            id, commission_id, assignment_id, attempt_id, result_id,
            skill_name, content_digest, worker_configuration, harness,
            assignment_class, observation, verification_outcome, corrections,
            cost_cents, latency_ms, principal_intervention, evidence_ids_json,
            scope_json, confidence_basis_points, observed_at
         ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9,
                   'required_skill_failure', 'uncertain', ?10, 0, ?11, ?12,
                   ?13, ?14, 6000, ?15)",
        params![
            Uuid::new_v4().to_string(),
            commission_id,
            assignment_id,
            attempt_id,
            skill_name,
            content_digest,
            worker_configuration,
            harness,
            assignment_class,
            corrections,
            observed_at_ms.saturating_sub(started_at_ms).max(0),
            principal_intervention,
            serde_json::to_string(&evidence)?,
            serde_json::to_string(&scope)?,
            observed_at_ms / 1000,
        ],
    )?;
    Ok(())
}

fn record_attempt_recovery(
    transaction: &Transaction<'_>,
    recovery: AttemptRecovery<'_>,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT OR IGNORE INTO attempt_recoveries (
            id, commission_id, assignment_id, attempt_id, cause, classification,
            equivalence_key, action, requirement, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            Uuid::new_v4().to_string(),
            recovery.commission_id,
            recovery.assignment_id,
            recovery.attempt_id,
            recovery.cause,
            recovery.classification,
            recovery.equivalence_key,
            recovery.action,
            recovery.requirement,
            unix_timestamp()?,
        ],
    )?;
    record_event_with_payload(
        transaction,
        recovery.commission_id,
        EventKind::RecoveryDecided,
        transaction.query_row(
            "SELECT revision FROM commissions WHERE id = ?1",
            [recovery.commission_id],
            |row| row.get::<_, i64>(0),
        )?,
        &serde_json::json!({
            "assignment_id": recovery.assignment_id,
            "attempt_id": recovery.attempt_id,
            "cause": recovery.cause,
            "classification": recovery.classification,
            "equivalence_key": recovery.equivalence_key,
            "action": recovery.action,
            "requirement": recovery.requirement,
        }),
    )?;
    Ok(())
}

fn authenticated_attachment_id(
    connection: &Connection,
    request: &Request,
) -> Result<String, TyrionError> {
    let attachment_token = request
        .attachment_token
        .as_deref()
        .filter(|attachment_token| !attachment_token.trim().is_empty())
        .ok_or_else(|| {
            TyrionError::ControlDenied(
                "an authenticated Attachment is required for this Commission operation".into(),
            )
        })?;
    let token_hash = attachment_token_hash(attachment_token);
    connection
        .query_row(
            "SELECT id FROM attachments WHERE session_token_hash = ?1",
            [token_hash],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| TyrionError::ControlDenied("Attachment credential is invalid".into()))
}

fn authenticate_principal(request: &Request, expected_token_hash: &str) -> Result<(), TyrionError> {
    let token = request
        .principal_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            TyrionError::ControlDenied(
                "the independent Principal control credential is required".into(),
            )
        })?;
    let actual_token_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
    if actual_token_hash != expected_token_hash {
        return Err(TyrionError::ControlDenied(
            "the Principal control credential is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_reusable_preference(preference: &ReusablePreference) -> Result<(), TyrionError> {
    let statement = &preference.statement;
    let token_upper_bound = token_upper_bound(statement);
    let sentence_boundaries = statement
        .char_indices()
        .filter(|(index, character)| {
            matches!(character, '.' | '!' | '?')
                && statement[*index + character.len_utf8()..]
                    .trim_start()
                    .chars()
                    .next()
                    .is_some()
        })
        .count();
    if statement.trim() != statement
        || statement.is_empty()
        || statement.contains(['\n', '\r', '\0'])
        || statement.contains([';', ',', ':'])
        || sentence_boundaries > 0
        || contains_atomicity_boundary(statement)
        || token_upper_bound > 80
    {
        return Err(TyrionError::InvalidRequest(
            "a reusable preference must be one atomic sentence of at most 80 tokens under conservative UTF-8 accounting"
                .into(),
        ));
    }
    Ok(())
}

fn contains_atomicity_boundary(statement: &str) -> bool {
    const BOUNDARIES: &[&str] = &[
        " and ",
        " or ",
        " while ",
        " also ",
        " but ",
        " whereas ",
        " plus ",
    ];
    let normalized = statement.to_lowercase();
    BOUNDARIES
        .iter()
        .any(|boundary| normalized.contains(boundary))
}

fn token_upper_bound(value: &str) -> u64 {
    value.len() as u64
}

fn preference_fingerprint(statement: &str) -> String {
    let normalized = statement
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("sha256:{:x}", Sha256::digest(normalized.as_bytes()))
}

fn refresh_profile_claim_derived_data(
    transaction: &Transaction<'_>,
    claim_id: &str,
    statement_fingerprint: &str,
    now: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO profile_claim_indexes (claim_id, statement_fingerprint, indexed_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(claim_id) DO UPDATE SET
             statement_fingerprint = excluded.statement_fingerprint,
             indexed_at = excluded.indexed_at",
        params![claim_id, statement_fingerprint, now],
    )?;
    transaction.execute(
        "INSERT INTO profile_claim_caches (claim_id, cached_projection_json, cached_at)
         VALUES (?1, '{}', ?2)
         ON CONFLICT(claim_id) DO UPDATE SET
             cached_projection_json = excluded.cached_projection_json,
             cached_at = excluded.cached_at",
        params![claim_id, now],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn make_room_in_active_profile(
    transaction: &Transaction<'_>,
    scope_kind: &str,
    scope_id: Option<&str>,
    claim_limit: u64,
    token_limit: u64,
    incoming_tokens: u64,
    excluded_claim_id: Option<&str>,
    now: i64,
) -> Result<Vec<String>, TyrionError> {
    let mut demoted_claim_ids = Vec::new();
    loop {
        let (active_claims, active_tokens) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(profile_claim_versions.token_upper_bound), 0)
             FROM profile_claims
             JOIN profile_claim_versions
               ON profile_claim_versions.claim_id = profile_claims.id
              AND profile_claim_versions.version = profile_claims.current_version
             WHERE lifecycle_state = 'active' AND scope_kind = ?1
               AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))
               AND id != COALESCE(?3, '')",
            params![scope_kind, scope_id, excluded_claim_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        if active_claims.saturating_add(1) <= claim_limit
            && active_tokens.saturating_add(incoming_tokens) <= token_limit
        {
            return Ok(demoted_claim_ids);
        }
        let demotion = transaction
            .query_row(
                "SELECT id FROM profile_claims
                 WHERE lifecycle_state = 'active' AND strength = 'soft'
                   AND scope_kind = ?1
                   AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))
                   AND id != COALESCE(?3, '')
                 ORDER BY confidence_basis_points, updated_at, created_at, id
                 LIMIT 1",
                params![scope_kind, scope_id, excluded_claim_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(demotion) = demotion else {
            return Err(TyrionError::InvalidRequest(format!(
                "the {scope_kind} Profile cannot admit this preference without truncating hard Profile Claims"
            )));
        };
        transaction.execute(
            "UPDATE profile_claims
             SET lifecycle_state = 'candidate', lifecycle_changed_at = ?2,
                 updated_at = ?2
             WHERE id = ?1",
            params![demotion, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (?1, 'active', 'candidate', 'active_profile_capacity', ?2)",
            params![demotion, now],
        )?;
        demoted_claim_ids.push(demotion);
    }
}

fn reconcile_active_profile_budget(
    transaction: &Transaction<'_>,
    scope_kind: &str,
    scope_id: Option<&str>,
    claim_limit: u64,
    token_limit: u64,
    now: i64,
) -> Result<Vec<String>, TyrionError> {
    let mut demoted_claim_ids = Vec::new();
    loop {
        let (active_claims, active_tokens) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(profile_claim_versions.token_upper_bound), 0)
             FROM profile_claims
             JOIN profile_claim_versions
               ON profile_claim_versions.claim_id = profile_claims.id
              AND profile_claim_versions.version = profile_claims.current_version
             WHERE lifecycle_state = 'active' AND scope_kind = ?1
               AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))",
            params![scope_kind, scope_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        if active_claims <= claim_limit && active_tokens <= token_limit {
            return Ok(demoted_claim_ids);
        }
        let demotion = transaction
            .query_row(
                "SELECT id FROM profile_claims
                 WHERE lifecycle_state = 'active' AND strength = 'soft'
                   AND scope_kind = ?1
                   AND (scope_id = ?2 OR (scope_id IS NULL AND ?2 IS NULL))
                 ORDER BY confidence_basis_points, updated_at, created_at, id
                 LIMIT 1",
                params![scope_kind, scope_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(demotion) = demotion else {
            return Err(TyrionError::InvalidRequest(format!(
                "memory import exceeds the {scope_kind} Profile budget and cannot truncate hard Profile Claims"
            )));
        };
        transaction.execute(
            "UPDATE profile_claims
             SET lifecycle_state = 'candidate', lifecycle_changed_at = ?2,
                 updated_at = ?2
             WHERE id = ?1",
            params![demotion, now],
        )?;
        transaction.execute(
            "INSERT INTO profile_claim_lifecycle (
                claim_id, from_state, to_state, reason, changed_at
             ) VALUES (?1, 'active', 'candidate', 'memory_import_capacity', ?2)",
            params![demotion, now],
        )?;
        demoted_claim_ids.push(demotion);
    }
}

fn hard_profile_capacity_allows(
    transaction: &Transaction<'_>,
    scope_kind: &str,
    scope_id: Option<&str>,
    claim_limit: u64,
    token_limit: u64,
    incoming_tokens: u64,
) -> Result<bool, TyrionError> {
    let (hard_claims, hard_tokens) = transaction.query_row(
        "SELECT COUNT(*), COALESCE(SUM(profile_claim_versions.token_upper_bound), 0)
         FROM profile_claims
         JOIN profile_claim_versions
           ON profile_claim_versions.claim_id = profile_claims.id
          AND profile_claim_versions.version = profile_claims.current_version
         WHERE profile_claims.lifecycle_state = 'active'
           AND profile_claims.strength = 'hard' AND profile_claims.scope_kind = ?1
           AND (profile_claims.scope_id = ?2
                OR (profile_claims.scope_id IS NULL AND ?2 IS NULL))",
        params![scope_kind, scope_id],
        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
    )?;
    Ok(hard_claims.saturating_add(1) <= claim_limit
        && hard_tokens.saturating_add(incoming_tokens) <= token_limit)
}

fn commission_project_id(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<Option<String>, TyrionError> {
    transaction
        .query_row(
            "SELECT project_id FROM commissions WHERE id = ?1",
            [commission_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))
}

fn ensure_claim_scope(
    scope_kind: &str,
    scope_id: Option<&str>,
    source_project_id: Option<&str>,
) -> Result<(), TyrionError> {
    if scope_kind == "project" && scope_id != source_project_id {
        return Err(TyrionError::ControlDenied(
            "a project Profile Claim can only be controlled from the same verified Project".into(),
        ));
    }
    Ok(())
}

fn profile_scope(scope_kind: &str, scope_id: Option<&str>) -> Value {
    if scope_kind == "project" {
        serde_json::json!({"kind": scope_kind, "project_id": scope_id})
    } else {
        serde_json::json!({"kind": scope_kind})
    }
}

fn claim_observation_ids(
    transaction: &Transaction<'_>,
    claim_ids: &[String],
) -> Result<Vec<String>, TyrionError> {
    let mut observation_ids = HashSet::new();
    for claim_id in claim_ids {
        let mut statement = transaction
            .prepare("SELECT observation_id FROM profile_claim_observations WHERE claim_id = ?1")?;
        let rows = statement.query_map([claim_id], |row| row.get::<_, String>(0))?;
        for observation_id in rows {
            observation_ids.insert(observation_id?);
        }
    }
    let mut observation_ids = observation_ids.into_iter().collect::<Vec<_>>();
    observation_ids.sort();
    Ok(observation_ids)
}

fn claim_observation_project_ids(
    transaction: &Transaction<'_>,
    claim_id: &str,
) -> Result<Vec<String>, TyrionError> {
    let mut statement = transaction.prepare(
        "SELECT DISTINCT learning_observations.project_id
         FROM learning_observations
         JOIN profile_claim_observations
           ON profile_claim_observations.observation_id = learning_observations.id
         WHERE profile_claim_observations.claim_id = ?1
         ORDER BY learning_observations.project_id",
    )?;
    let project_ids = statement
        .query_map([claim_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(project_ids)
}

fn blocking_learning_boundary_id(
    transaction: &Transaction<'_>,
    statement_fingerprint: &str,
    claim_scope_kind: &str,
    claim_scope_id: Option<&str>,
    related_project_ids: &[String],
) -> Result<Option<String>, TyrionError> {
    let mut statement = transaction.prepare(
        "SELECT id, scope_kind, scope_id FROM learning_boundaries
         WHERE statement_fingerprint = ?1
         ORDER BY CASE scope_kind WHEN 'project' THEN 0 ELSE 1 END, created_at, id",
    )?;
    let boundaries = statement
        .query_map([statement_fingerprint], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(boundaries
        .into_iter()
        .find(|(_, boundary_scope_kind, boundary_scope_id)| {
            boundary_scope_kind == "principal"
                || (boundary_scope_kind == "project"
                    && (claim_scope_kind == "principal"
                        || claim_scope_kind == "project"
                            && boundary_scope_id.as_deref() == claim_scope_id
                        || boundary_scope_id
                            .as_ref()
                            .is_some_and(|project_id| related_project_ids.contains(project_id))))
        })
        .map(|(id, _, _)| id))
}

fn dedicated_observation_ids(
    transaction: &Transaction<'_>,
    observation_ids: &[String],
    claims_to_delete: &[String],
) -> Result<Vec<String>, TyrionError> {
    let deleting = claims_to_delete.iter().collect::<HashSet<_>>();
    let mut dedicated = Vec::new();
    for observation_id in observation_ids {
        let mut statement = transaction
            .prepare("SELECT claim_id FROM profile_claim_observations WHERE observation_id = ?1")?;
        let linked_claim_ids = statement
            .query_map([observation_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if linked_claim_ids
            .iter()
            .all(|linked_claim_id| deleting.contains(linked_claim_id))
        {
            dedicated.push(observation_id.clone());
        }
    }
    Ok(dedicated)
}

fn related_claim_ids(
    transaction: &Transaction<'_>,
    statement_fingerprint: &str,
    observation_ids: &[String],
    claims_to_delete: &[String],
) -> Result<Vec<String>, TyrionError> {
    let mut related = HashSet::new();
    {
        let mut statement = transaction
            .prepare("SELECT id FROM profile_claims WHERE statement_fingerprint = ?1")?;
        for claim_id in
            statement.query_map([statement_fingerprint], |row| row.get::<_, String>(0))?
        {
            related.insert(claim_id?);
        }
    }
    for observation_id in observation_ids {
        let mut statement = transaction
            .prepare("SELECT claim_id FROM profile_claim_observations WHERE observation_id = ?1")?;
        for claim_id in statement.query_map([observation_id], |row| row.get::<_, String>(0))? {
            related.insert(claim_id?);
        }
    }
    for claim_id in claims_to_delete {
        related.remove(claim_id);
    }
    let mut related = related.into_iter().collect::<Vec<_>>();
    related.sort();
    Ok(related)
}

fn count_for_claims(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
    claim_ids: &[String],
) -> Result<u64, TyrionError> {
    let allowed = [
        ("profile_claim_versions", "claim_id"),
        ("attempt_profile_claims", "claim_id"),
        ("imported_profile_claim_attempts", "claim_id"),
        ("profile_claim_indexes", "claim_id"),
        ("profile_claim_caches", "claim_id"),
    ];
    if !allowed.contains(&(table, column)) {
        return Err(TyrionError::InvalidRequest(
            "invalid internal Profile Claim count target".into(),
        ));
    }
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
    let mut total = 0_u64;
    for claim_id in claim_ids {
        total = total
            .saturating_add(transaction.query_row(&sql, [claim_id], |row| row.get::<_, u64>(0))?);
    }
    Ok(total)
}

fn exact_scope_values(value: &Value, scope_kind: &str, scope_id: Option<&str>) -> Vec<Value> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry["scope"]["kind"] == scope_kind
                && (scope_kind == "principal" || entry["scope"]["project_id"].as_str() == scope_id)
        })
        .cloned()
        .collect()
}

fn validate_memory_bundle(bundle: &Value) -> Result<(), TyrionError> {
    if contains_prohibited_memory_key(bundle) {
        return Err(TyrionError::InvalidRequest(
            "memory import contains a prohibited secret or credential field".into(),
        ));
    }
    reject_unknown_memory_fields(
        bundle,
        &[
            "format",
            "version",
            "scope",
            "exported_at",
            "checksum",
            "data",
            "summary_markdown",
        ],
        "memory export",
    )?;
    if bundle["format"] != "tyrion.memory" || bundle["version"] != 1 {
        return Err(TyrionError::InvalidRequest(
            "memory import requires tyrion.memory version 1".into(),
        ));
    }
    required_json_i64(bundle, "exported_at")?;
    validate_memory_scope(&bundle["scope"])?;
    let data = bundle.get("data").ok_or_else(|| {
        TyrionError::InvalidRequest("memory import is missing structured data".into())
    })?;
    reject_unknown_memory_fields(
        data,
        &[
            "claims",
            "learning_boundaries",
            "deletion_receipts",
            "commission_records",
            "excluded_categories",
        ],
        "memory data",
    )?;
    for claim in required_json_array(data, "claims")? {
        validate_memory_claim_entry(claim)?;
    }
    for boundary in required_json_array(data, "learning_boundaries")? {
        validate_memory_boundary(boundary)?;
    }
    for receipt in required_json_array(data, "deletion_receipts")? {
        validate_memory_deletion_receipt(receipt)?;
    }
    for record in required_json_array(data, "commission_records")? {
        validate_memory_commission_record(record)?;
    }
    if required_json_array(data, "excluded_categories")?
        .iter()
        .any(|category| !category.is_string())
    {
        return Err(TyrionError::InvalidRequest(
            "memory excluded_categories must contain only strings".into(),
        ));
    }
    let checksum = required_json_str(bundle, "checksum")?;
    let actual_checksum = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(data)?));
    if checksum != actual_checksum {
        return Err(TyrionError::InvalidRequest(
            "memory import checksum does not match its structured data".into(),
        ));
    }
    let summary = bundle["summary_markdown"].as_str().ok_or_else(|| {
        TyrionError::InvalidRequest("memory import requires a readable Markdown summary".into())
    })?;
    let mut summary_identifiers = vec![checksum];
    summary_identifiers.extend(
        required_json_array(data, "claims")?
            .iter()
            .filter_map(|entry| entry["claim"]["id"].as_str()),
    );
    for collection in [
        "learning_boundaries",
        "deletion_receipts",
        "commission_records",
    ] {
        summary_identifiers.extend(
            required_json_array(data, collection)?
                .iter()
                .filter_map(|entry| entry["id"].as_str()),
        );
    }
    if summary_identifiers
        .into_iter()
        .any(|identifier| !summary.contains(identifier))
    {
        return Err(TyrionError::InvalidRequest(
            "memory Markdown summary omits a structured identifier or checksum".into(),
        ));
    }
    Ok(())
}

fn validate_memory_claim_entry(entry: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        entry,
        &[
            "claim",
            "versions",
            "observations",
            "lifecycle_history",
            "affected_attempts",
            "retention",
        ],
        "memory claim entry",
    )?;
    let claim = entry
        .get("claim")
        .ok_or_else(|| TyrionError::InvalidRequest("memory claim entry is missing claim".into()))?;
    reject_unknown_memory_fields(
        claim,
        &[
            "id",
            "version",
            "statement",
            "token_upper_bound",
            "token_accounting",
            "strength",
            "scope",
            "applicability",
            "provenance",
            "confidence",
            "lifecycle",
            "created_at",
            "updated_at",
        ],
        "memory claim",
    )?;
    required_json_str(claim, "id")?;
    let current_version = required_json_i64(claim, "version")?;
    let current_statement = required_json_str(claim, "statement")?;
    if required_json_u64(claim, "token_upper_bound")? != token_upper_bound(current_statement) {
        return Err(TyrionError::InvalidRequest(
            "memory claim head token accounting does not match its statement".into(),
        ));
    }
    if claim["token_accounting"] != "utf8_byte_upper_bound" {
        return Err(TyrionError::InvalidRequest(
            "memory claim uses unsupported token accounting".into(),
        ));
    }
    let strength = required_json_str(claim, "strength")?;
    if !matches!(strength, "hard" | "soft") {
        return Err(TyrionError::InvalidRequest(
            "memory claim strength must be hard or soft".into(),
        ));
    }
    validate_memory_scope(&claim["scope"])?;
    reject_unknown_memory_fields(&claim["applicability"], &["work_kind"], "applicability")?;
    if claim["applicability"]["work_kind"] != "software_building" {
        return Err(TyrionError::InvalidRequest(
            "memory claim applicability must be software_building".into(),
        ));
    }
    validate_claim_version_provenance(&claim["provenance"])?;
    reject_unknown_memory_fields(
        &claim["confidence"],
        &["category", "basis_points"],
        "confidence",
    )?;
    let confidence = required_json_str(&claim["confidence"], "category")?;
    if !matches!(confidence, "explicit" | "inferred")
        || (strength == "hard") != (confidence == "explicit")
    {
        return Err(TyrionError::InvalidRequest(
            "memory claim strength and confidence are inconsistent".into(),
        ));
    }
    if required_json_u64(&claim["confidence"], "basis_points")? > 10_000 {
        return Err(TyrionError::InvalidRequest(
            "memory claim confidence exceeds 10000 basis points".into(),
        ));
    }
    reject_unknown_memory_fields(&claim["lifecycle"], &["state"], "claim lifecycle")?;
    let lifecycle_state = required_json_str(&claim["lifecycle"], "state")?;
    if !matches!(
        lifecycle_state,
        "candidate" | "active" | "suppressed" | "contradicted"
    ) {
        return Err(TyrionError::InvalidRequest(
            "memory claim lifecycle state is invalid".into(),
        ));
    }
    required_json_i64(claim, "created_at")?;
    required_json_i64(claim, "updated_at")?;

    let versions = required_json_array(entry, "versions")?;
    if versions.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "memory Profile Claim must contain at least one version".into(),
        ));
    }
    let mut current_matches = 0;
    let mut version_numbers = HashSet::new();
    for version in versions {
        reject_unknown_memory_fields(
            version,
            &[
                "version",
                "statement",
                "token_upper_bound",
                "token_accounting",
                "provenance",
                "disposition",
                "created_at",
            ],
            "memory claim version",
        )?;
        let version_number = required_json_i64(version, "version")?;
        if version_number <= 0 || !version_numbers.insert(version_number) {
            return Err(TyrionError::InvalidRequest(
                "memory claim versions must use unique positive numbers".into(),
            ));
        }
        let statement = required_json_str(version, "statement")?;
        if required_json_u64(version, "token_upper_bound")? != token_upper_bound(statement)
            || version["token_accounting"] != "utf8_byte_upper_bound"
        {
            return Err(TyrionError::InvalidRequest(
                "memory claim token accounting does not match its statement".into(),
            ));
        }
        validate_claim_version_provenance(&version["provenance"])?;
        let disposition = required_json_str(version, "disposition")?;
        if version_number == current_version {
            current_matches += 1;
            if statement != current_statement || disposition != "current" {
                return Err(TyrionError::InvalidRequest(
                    "memory claim head does not match its current immutable version".into(),
                ));
            }
        } else if disposition != "superseded" {
            return Err(TyrionError::InvalidRequest(
                "memory claim version disposition is inconsistent".into(),
            ));
        }
        required_json_i64(version, "created_at")?;
    }
    if current_matches != 1 {
        return Err(TyrionError::InvalidRequest(
            "memory claim must identify exactly one current version".into(),
        ));
    }
    if version_numbers.len() != current_version as usize
        || !(1..=current_version).all(|version| version_numbers.contains(&version))
    {
        return Err(TyrionError::InvalidRequest(
            "memory claim versions must form a complete sequence".into(),
        ));
    }

    let observations = required_json_array(entry, "observations")?;
    let mut observation_ids = HashSet::new();
    for observation in observations {
        validate_memory_observation(observation)?;
        observation_ids.insert(required_json_str(observation, "id")?);
    }
    let lifecycle_history = required_json_array(entry, "lifecycle_history")?;
    if lifecycle_history.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "memory claim requires lifecycle history".into(),
        ));
    }
    let mut prior_state: Option<&str> = None;
    let mut prior_sequence = None;
    for lifecycle in lifecycle_history {
        reject_unknown_memory_fields(
            lifecycle,
            &[
                "sequence",
                "from_state",
                "to_state",
                "reason",
                "observation_id",
                "changed_at",
            ],
            "memory lifecycle history",
        )?;
        let sequence = required_json_i64(lifecycle, "sequence")?;
        if prior_sequence.is_some_and(|prior| sequence <= prior) {
            return Err(TyrionError::InvalidRequest(
                "memory lifecycle sequence must increase".into(),
            ));
        }
        prior_sequence = Some(sequence);
        let from_state = lifecycle["from_state"].as_str();
        if lifecycle["from_state"].is_null() {
            if prior_state.is_some() {
                return Err(TyrionError::InvalidRequest(
                    "memory lifecycle transition unexpectedly resets its prior state".into(),
                ));
            }
        } else if from_state != prior_state {
            return Err(TyrionError::InvalidRequest(
                "memory lifecycle transition chain is inconsistent".into(),
            ));
        }
        let to_state = required_json_str(lifecycle, "to_state")?;
        if !matches!(
            to_state,
            "candidate" | "active" | "suppressed" | "contradicted"
        ) {
            return Err(TyrionError::InvalidRequest(
                "memory lifecycle transition has an invalid state".into(),
            ));
        }
        prior_state = Some(to_state);
        required_json_str(lifecycle, "reason")?;
        required_json_i64(lifecycle, "changed_at")?;
        if let Some(observation_id) = lifecycle["observation_id"].as_str() {
            if !observation_ids.contains(observation_id) {
                return Err(TyrionError::InvalidRequest(
                    "memory lifecycle transition references an unknown observation".into(),
                ));
            }
        } else if !lifecycle["observation_id"].is_null() {
            return Err(TyrionError::InvalidRequest(
                "memory lifecycle observation_id must be a string or null".into(),
            ));
        }
    }
    if lifecycle_history
        .last()
        .and_then(|entry| entry["to_state"].as_str())
        != Some(lifecycle_state)
    {
        return Err(TyrionError::InvalidRequest(
            "memory lifecycle history does not match the claim head".into(),
        ));
    }
    let mut attempt_ids = HashSet::new();
    for attempt in required_json_array(entry, "affected_attempts")? {
        reject_unknown_memory_fields(
            attempt,
            &[
                "claim_version",
                "attempt_id",
                "assignment_id",
                "commission_id",
                "result_id",
                "outcome",
                "recorded_at",
            ],
            "affected Attempt",
        )?;
        let claim_version = required_json_i64(attempt, "claim_version")?;
        if !version_numbers.contains(&claim_version) {
            return Err(TyrionError::InvalidRequest(
                "affected Attempt references a missing claim version".into(),
            ));
        }
        let attempt_id = required_json_str(attempt, "attempt_id")?;
        if !attempt_ids.insert(attempt_id) {
            return Err(TyrionError::InvalidRequest(format!(
                "memory claim repeats affected Attempt {attempt_id}"
            )));
        }
        required_json_str(attempt, "assignment_id")?;
        required_json_str(attempt, "commission_id")?;
        validate_optional_memory_string(attempt, "result_id")?;
        if let Some(outcome) = attempt["outcome"].as_str() {
            if !matches!(outcome, "accepted" | "edited" | "rejected" | "contradicted") {
                return Err(TyrionError::InvalidRequest(
                    "affected Attempt outcome is invalid".into(),
                ));
            }
        } else if !attempt["outcome"].is_null() {
            return Err(TyrionError::InvalidRequest(
                "affected Attempt outcome must be a string or null".into(),
            ));
        }
        validate_optional_memory_i64(attempt, "recorded_at")?;
    }
    reject_unknown_memory_fields(
        &entry["retention"],
        &["last_nonweak_support_at", "lifecycle_changed_at"],
        "claim retention",
    )?;
    validate_optional_memory_i64(&entry["retention"], "last_nonweak_support_at")?;
    required_json_i64(&entry["retention"], "lifecycle_changed_at")?;
    Ok(())
}

fn validate_memory_observation(observation: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        observation,
        &[
            "id",
            "commission_id",
            "project_id",
            "claim_id",
            "statement",
            "statement_fingerprint",
            "kind",
            "explanation",
            "strength",
            "observed_at",
            "provenance",
        ],
        "learning observation",
    )?;
    required_json_str(observation, "id")?;
    let commission_id = required_json_str(observation, "commission_id")?;
    let project_id = required_json_str(observation, "project_id")?;
    let statement = required_json_str(observation, "statement")?;
    let fingerprint = required_json_str(observation, "statement_fingerprint")?;
    if fingerprint != preference_fingerprint(statement) {
        return Err(TyrionError::InvalidRequest(
            "learning observation fingerprint does not match its statement".into(),
        ));
    }
    let kind = required_json_str(observation, "kind")?;
    validate_optional_memory_string(observation, "claim_id")?;
    validate_optional_memory_string(observation, "explanation")?;
    let strength = required_json_str(observation, "strength")?;
    let valid_pair = matches!(
        (kind, strength),
        ("principal_edit" | "explained_rejection", "strong")
            | ("unedited_acceptance", "weak")
            | ("contradiction", "contradiction")
    );
    if !valid_pair {
        return Err(TyrionError::InvalidRequest(
            "learning observation kind and strength are inconsistent".into(),
        ));
    }
    if matches!(kind, "explained_rejection" | "contradiction")
        && observation["explanation"]
            .as_str()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(TyrionError::InvalidRequest(
            "learning observation requires an explanation".into(),
        ));
    }
    required_json_i64(observation, "observed_at")?;
    reject_unknown_memory_fields(
        &observation["provenance"],
        &["commission_id", "project_id"],
        "observation provenance",
    )?;
    if required_json_str(&observation["provenance"], "commission_id")? != commission_id
        || required_json_str(&observation["provenance"], "project_id")? != project_id
    {
        return Err(TyrionError::InvalidRequest(
            "learning observation provenance does not match its identifiers".into(),
        ));
    }
    Ok(())
}

fn validate_claim_version_provenance(provenance: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        provenance,
        &["kind", "commission_id", "attachment_id"],
        "claim provenance",
    )?;
    required_json_str(provenance, "kind")?;
    required_json_str(provenance, "commission_id")?;
    required_json_str(provenance, "attachment_id")?;
    Ok(())
}

fn validate_memory_boundary(boundary: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        boundary,
        &["id", "scope", "statement_fingerprint", "created_at"],
        "Learning Boundary",
    )?;
    required_json_str(boundary, "id")?;
    validate_memory_scope(&boundary["scope"])?;
    required_json_str(boundary, "statement_fingerprint")?;
    required_json_i64(boundary, "created_at")?;
    Ok(())
}

fn validate_memory_deletion_receipt(receipt: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        receipt,
        &[
            "id",
            "claim_id",
            "scope",
            "cascade",
            "remaining_related_claim_ids",
            "deleted_at",
        ],
        "deletion receipt",
    )?;
    required_json_str(receipt, "id")?;
    required_json_str(receipt, "claim_id")?;
    validate_memory_scope(&receipt["scope"])?;
    reject_unknown_memory_fields(
        &receipt["cascade"],
        &[
            "claims",
            "claim_versions",
            "supporting_observations",
            "dedicated_excerpts",
            "affected_attempt_records",
            "indexes",
            "caches",
        ],
        "deletion cascade",
    )?;
    for field in [
        "claims",
        "claim_versions",
        "supporting_observations",
        "dedicated_excerpts",
        "affected_attempt_records",
        "indexes",
        "caches",
    ] {
        required_json_u64(&receipt["cascade"], field)?;
    }
    if receipt["remaining_related_claim_ids"]
        .as_array()
        .is_none_or(|ids| ids.iter().any(|id| !id.is_string()))
    {
        return Err(TyrionError::InvalidRequest(
            "deletion receipt related claim identifiers must be an array of strings".into(),
        ));
    }
    required_json_i64(receipt, "deleted_at")?;
    Ok(())
}

fn validate_memory_commission_record(record: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(
        record,
        &[
            "id",
            "project_id",
            "status",
            "revision",
            "created_at",
            "accepted_at",
            "completed_at",
            "artifact_revision",
            "retained_results",
            "retained_evidence",
        ],
        "Commission Record",
    )?;
    required_json_str(record, "id")?;
    validate_optional_memory_string(record, "project_id")?;
    let status = required_json_str(record, "status")?;
    if !matches!(
        status,
        "proposed" | "active" | "paused" | "cancelled" | "verified_complete"
    ) {
        return Err(TyrionError::InvalidRequest(
            "Commission Record status is invalid".into(),
        ));
    }
    required_json_i64(record, "revision")?;
    required_json_i64(record, "created_at")?;
    validate_optional_memory_i64(record, "accepted_at")?;
    validate_optional_memory_i64(record, "completed_at")?;
    validate_optional_memory_string(record, "artifact_revision")?;
    required_json_u64(record, "retained_results")?;
    required_json_u64(record, "retained_evidence")?;
    Ok(())
}

fn validate_memory_scope(scope: &Value) -> Result<(), TyrionError> {
    reject_unknown_memory_fields(scope, &["kind", "project_id"], "memory scope")?;
    match required_json_str(scope, "kind")? {
        "principal" if scope.get("project_id").is_none() => Ok(()),
        "project" if scope["project_id"].as_str().is_some() => Ok(()),
        _ => Err(TyrionError::InvalidRequest(
            "memory scope must be principal or name one project".into(),
        )),
    }
}

fn reject_unknown_memory_fields(
    value: &Value,
    allowed: &[&str],
    context: &str,
) -> Result<(), TyrionError> {
    let object = value
        .as_object()
        .ok_or_else(|| TyrionError::InvalidRequest(format!("{context} must be an object")))?;
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(TyrionError::InvalidRequest(format!(
            "{context} contains unknown field {unknown}"
        )));
    }
    Ok(())
}

fn validate_optional_memory_string(value: &Value, field: &str) -> Result<(), TyrionError> {
    if !value[field].is_null() && !value[field].is_string() {
        return Err(TyrionError::InvalidRequest(format!(
            "memory import field {field} must be a string or null"
        )));
    }
    Ok(())
}

fn validate_optional_memory_i64(value: &Value, field: &str) -> Result<(), TyrionError> {
    if !value[field].is_null() && value[field].as_i64().is_none() {
        return Err(TyrionError::InvalidRequest(format!(
            "memory import field {field} must be an integer or null"
        )));
    }
    Ok(())
}

fn contains_prohibited_memory_key(value: &Value) -> bool {
    const PROHIBITED: &[&str] = &[
        "credential",
        "credential_grant",
        "credential_grants",
        "credential_value",
        "api_key",
        "password",
        "access_token",
        "refresh_token",
        "authorization",
        "private_key",
        "secret",
        "raw_secret",
        "principal_control_token",
        "attachment_token",
        "session_token",
        "session_token_hash",
        "approval_artifact",
    ];
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            PROHIBITED.contains(&key.as_str()) || contains_prohibited_memory_key(value)
        }),
        Value::Array(values) => values.iter().any(contains_prohibited_memory_key),
        _ => false,
    }
}

fn required_json_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, TyrionError> {
    value[field].as_str().ok_or_else(|| {
        TyrionError::InvalidRequest(format!("memory import field {field} must be a string"))
    })
}

fn required_json_i64(value: &Value, field: &str) -> Result<i64, TyrionError> {
    value[field].as_i64().ok_or_else(|| {
        TyrionError::InvalidRequest(format!("memory import field {field} must be an integer"))
    })
}

fn required_json_u64(value: &Value, field: &str) -> Result<u64, TyrionError> {
    value[field].as_u64().ok_or_else(|| {
        TyrionError::InvalidRequest(format!(
            "memory import field {field} must be a non-negative integer"
        ))
    })
}

fn required_json_array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value], TyrionError> {
    value[field].as_array().map(Vec::as_slice).ok_or_else(|| {
        TyrionError::InvalidRequest(format!("memory import field {field} must be an array"))
    })
}

fn imported_commission_id(checksum: &str, source_commission_id: &str) -> String {
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{checksum}:{source_commission_id}").as_bytes())
    );
    format!("memory-import-{}", &digest[..32])
}

fn export_commission_records(
    connection: &Connection,
    scope_kind: &str,
    scope_id: Option<&str>,
    referenced_commission_ids: &HashSet<String>,
) -> Result<Vec<Value>, TyrionError> {
    let mut records = Vec::new();
    let mut record_ids = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT id, project_id, status, revision, created_at, accepted_at,
                completed_at, artifact_revision,
                (SELECT COUNT(*) FROM results
                 JOIN attempts ON attempts.id = results.attempt_id
                 JOIN assignments ON assignments.id = attempts.assignment_id
                 WHERE assignments.commission_id = commissions.id),
                (SELECT COUNT(*) FROM evidence
                 WHERE evidence.commission_id = commissions.id)
         FROM commissions
         WHERE id NOT LIKE 'memory-import-%'
         ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "project_id": row.get::<_, Option<String>>(1)?,
            "status": row.get::<_, String>(2)?,
            "revision": row.get::<_, i64>(3)?,
            "created_at": row.get::<_, i64>(4)?,
            "accepted_at": row.get::<_, Option<i64>>(5)?,
            "completed_at": row.get::<_, Option<i64>>(6)?,
            "artifact_revision": row.get::<_, Option<String>>(7)?,
            "retained_results": row.get::<_, u64>(8)?,
            "retained_evidence": row.get::<_, u64>(9)?,
        }))
    })?;
    for record in rows {
        let record = record?;
        let record_id = record["id"].as_str().unwrap_or_default();
        let record_project_id = record["project_id"].as_str();
        let in_scope = scope_kind == "principal" && record_project_id.is_none()
            || scope_kind == "project" && record_project_id == scope_id;
        if in_scope || referenced_commission_ids.contains(record_id) {
            record_ids.insert(record_id.to_owned());
            records.push(record);
        }
    }
    let mut statement = connection.prepare(
        "SELECT record_id, record_json FROM imported_commission_records
         ORDER BY imported_at, record_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (record_id, record_json) = row?;
        let record = serde_json::from_str::<Value>(&record_json)?;
        let record_project_id = record["project_id"].as_str();
        let in_scope = scope_kind == "principal" && record_project_id.is_none()
            || scope_kind == "project" && record_project_id == scope_id;
        if (in_scope || referenced_commission_ids.contains(&record_id))
            && record_ids.insert(record_id)
        {
            records.push(record);
        }
    }
    Ok(records)
}

fn record_result_profile_claim_outcome(
    transaction: &Transaction<'_>,
    result_id: &str,
    outcome: ProfileClaimOutcome,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE attempt_profile_claims
         SET result_id = ?1, outcome = ?2, recorded_at = ?3
         WHERE attempt_id = (SELECT attempt_id FROM results WHERE id = ?1)
           AND (
               outcome IS NULL
               OR ?2 = 'contradicted'
               OR (?2 = 'accepted' AND outcome != 'contradicted')
           )",
        params![result_id, outcome.as_str(), unix_timestamp()?],
    )?;
    Ok(())
}

fn record_attempt_profile_claim_outcome(
    transaction: &Transaction<'_>,
    attempt_id: &str,
    outcome: ProfileClaimOutcome,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE attempt_profile_claims
         SET result_id = (
                 SELECT id FROM results WHERE attempt_id = ?1
                 ORDER BY created_at DESC, rowid DESC LIMIT 1
             ),
             outcome = ?2, recorded_at = ?3
         WHERE attempt_id = ?1 AND outcome IS NULL",
        params![attempt_id, outcome.as_str(), unix_timestamp()?],
    )?;
    Ok(())
}

fn record_superseded_profile_claims_as_edited(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    replacement_attempt_id: &str,
) -> Result<(), TyrionError> {
    transaction.execute(
        "UPDATE attempt_profile_claims
         SET outcome = ?4, recorded_at = ?3
         WHERE outcome = 'rejected' AND result_id IS NOT NULL
           AND attempt_id != ?2
           AND attempt_id IN (
               SELECT attempts.id FROM attempts
               JOIN results ON results.attempt_id = attempts.id
               WHERE attempts.assignment_id = ?1 AND results.status = 'superseded'
           )",
        params![
            assignment_id,
            replacement_attempt_id,
            unix_timestamp()?,
            ProfileClaimOutcome::Edited.as_str()
        ],
    )?;
    Ok(())
}

fn build_worker_context_packet(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ready: &ReadyAssignmentDispatch,
    configuration: &Value,
    criteria: &[worker::CriterionDefinition],
    authority: &AuthorityEnvelope,
    execution: &ExecutionSpec,
) -> Result<WorkerContextSelection, TyrionError> {
    let (principal_instruction, project_id, constraints_json) = transaction.query_row(
        "SELECT goal, project_id, commission_constraints_json
         FROM commissions WHERE id = ?1",
        [commission_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    let constraints = serde_json::from_str::<Value>(&constraints_json)?;
    let context_capacity = configuration["context"]["capacity_tokens"]
        .as_u64()
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "selected Worker Configuration is missing its context capacity".into(),
            )
        })?;
    let hard_max_tokens = 15_000_u64.min(context_capacity.saturating_mul(8) / 100);
    let token_budget = 2_000_u64.min(hard_max_tokens);
    let candidates = {
        let mut statement = transaction.prepare(
            "SELECT profile_claims.id, profile_claims.current_version
             FROM profile_claims
             JOIN profile_claim_versions
               ON profile_claim_versions.claim_id = profile_claims.id
              AND profile_claim_versions.version = profile_claims.current_version
             WHERE profile_claims.lifecycle_state = 'active'
               AND profile_claims.applicability = 'software_building'
               AND profile_claim_versions.provenance_commission_id != ?1
               AND (
                   profile_claims.scope_kind = 'principal'
                   OR (profile_claims.scope_kind = 'project'
                       AND profile_claims.scope_id = ?2 AND ?2 IS NOT NULL)
               )
             ORDER BY CASE profile_claims.scope_kind WHEN 'project' THEN 0 ELSE 1 END,
                      CASE profile_claims.strength WHEN 'hard' THEN 0 ELSE 1 END,
                      profile_claims.created_at, profile_claims.id",
        )?;
        let rows = statement.query_map(params![commission_id, project_id.as_deref()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut selected_claims = Vec::new();
    let mut advisory_claims = Vec::new();
    let mut tokens_used = serde_json::to_vec(&advisory_claims)?.len() as u64;
    for (claim_id, claim_version) in candidates {
        let mut claim = profile_claim(transaction, &claim_id)?;
        claim["advisory"] = Value::Bool(true);
        advisory_claims.push(claim);
        let candidate_tokens_used = serde_json::to_vec(&advisory_claims)?.len() as u64;
        if candidate_tokens_used > token_budget {
            advisory_claims.pop();
            continue;
        }
        selected_claims.push(ProfileClaimReference {
            id: claim_id,
            version: claim_version,
        });
        tokens_used = candidate_tokens_used;
    }
    let packet = WorkerContextPacket {
        version: 1,
        precedence: [
            "current_principal_instructions",
            "commission_constraints",
            "acceptance_criteria",
            "authority_envelope",
            "resource_ceilings",
            "current_repository_evidence",
            "advisory_profile_claims",
        ],
        binding: serde_json::json!({
            "current_principal_instructions": [principal_instruction],
            "commission_constraints": constraints,
            "acceptance_criteria": criteria,
            "authority_envelope": authority,
            "resource_ceilings": {
                "max_attempts": ready.max_attempts,
                "max_elapsed_seconds": ready.max_elapsed_seconds,
                "max_worker_concurrency": ready.max_worker_concurrency,
                "max_storage_bytes": ready.max_storage_bytes,
                "max_model_spend_cents": ready.max_model_spend_cents,
                "max_paid_service_spend_cents": ready.max_paid_service_spend_cents,
            },
            "current_repository_evidence": {
                "project_id": project_id,
                "execution": execution,
                "artifact_revision": ready.current_artifact_revision,
            },
        }),
        advisory: serde_json::json!({
            "profile_claims": advisory_claims,
            "instruction": "Profile Claims are advisory and yield to every binding source above.",
            "budget": {
                "target_tokens": 2_000,
                "hard_max_tokens": hard_max_tokens,
                "tokens_used": tokens_used,
                "complete_slice_limit_tokens": 15_000,
                "worker_context_fraction_basis_points": 800,
                "accounting": "utf8_byte_upper_bound",
            },
            "authority_effect": {
                "routing": false,
                "approval_gates": false,
                "credentials": false,
                "resource_ceilings": false,
            },
        }),
    };
    Ok(WorkerContextSelection {
        packet,
        claims: selected_claims,
        token_budget: hard_max_tokens,
        tokens_used,
    })
}

fn ensure_commission_exists(
    connection: &Connection,
    commission_id: &str,
) -> Result<(), TyrionError> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM commissions WHERE id = ?1)",
        [commission_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Err(TyrionError::NotFound(commission_id.to_owned()));
    }
    Ok(())
}

fn ensure_attachment_capability(
    connection: &Connection,
    attachment_id: &str,
    required_capability: &str,
) -> Result<(), TyrionError> {
    let capabilities_json = connection
        .query_row(
            "SELECT capabilities_json FROM attachments WHERE id = ?1",
            [attachment_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::ControlDenied(format!("Attachment {attachment_id} was not found"))
        })?;
    let capabilities = serde_json::from_str::<Vec<String>>(&capabilities_json)?;
    if !capabilities
        .iter()
        .any(|capability| capability == required_capability)
    {
        return Err(TyrionError::ControlDenied(format!(
            "Attachment {attachment_id} lacks the {required_capability} capability"
        )));
    }
    Ok(())
}

fn ensure_commission_attachment(
    connection: &Connection,
    attachment_id: &str,
    commission_id: &str,
    required_capability: &str,
) -> Result<(), TyrionError> {
    ensure_commission_exists(connection, commission_id)?;
    ensure_attachment_capability(connection, attachment_id, required_capability)?;
    let observes = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM commission_attachments
            WHERE commission_id = ?1 AND attachment_id = ?2
         )",
        params![commission_id, attachment_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !observes {
        return Err(TyrionError::ControlDenied(format!(
            "Attachment {attachment_id} does not observe Commission {commission_id}"
        )));
    }
    Ok(())
}

fn ensure_active_attachment(
    connection: &Connection,
    attachment_id: &str,
    commission_id: &str,
    required_capability: &str,
) -> Result<(), TyrionError> {
    ensure_commission_attachment(
        connection,
        attachment_id,
        commission_id,
        required_capability,
    )?;
    let is_active = connection.query_row(
        "SELECT role = 'active' FROM commission_attachments
         WHERE commission_id = ?1 AND attachment_id = ?2",
        params![commission_id, attachment_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !is_active {
        return Err(TyrionError::ControlDenied(format!(
            "Attachment {attachment_id} is not the Active Attachment for Commission {commission_id}"
        )));
    }
    Ok(())
}

fn apply_attachment_worker_controls(
    connection: &Connection,
    attachment_id: &str,
    commission_id: &str,
    projection: &mut Value,
    runtime: &worker::WorkerRuntime,
) -> Result<(), TyrionError> {
    let (role, capabilities_json) = connection.query_row(
        "SELECT commission_attachments.role, attachments.capabilities_json
         FROM commission_attachments
         JOIN attachments ON attachments.id = commission_attachments.attachment_id
         WHERE commission_attachments.commission_id = ?1
           AND commission_attachments.attachment_id = ?2",
        params![commission_id, attachment_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let capabilities = serde_json::from_str::<Vec<String>>(&capabilities_json)?;
    let active = role == "active";
    for worker in projection["workers"].as_array_mut().into_iter().flatten() {
        let mut controls = vec![Value::String("inspect".into())];
        let attempt_is_live = worker["attempt_id"]
            .as_str()
            .is_some_and(|attempt_id| runtime.is_attempt_active(attempt_id));
        let attempt_accepts_control = worker["attempt_id"]
            .as_str()
            .is_some_and(|attempt_id| runtime.accepts_live_control(attempt_id));
        if attempt_is_live {
            if let Some(telemetry) = worker["attempt_id"]
                .as_str()
                .and_then(|attempt_id| runtime.live_telemetry(attempt_id))
            {
                let telemetry_is_newer = telemetry["activity_at_ms"]
                    .as_i64()
                    .zip(worker["activity_at_ms"].as_i64())
                    .is_some_and(|(live, durable)| live > durable);
                if telemetry_is_newer {
                    for field in ["latest_meaningful_activity", "activity_at_ms"] {
                        if !telemetry[field].is_null() {
                            worker[field] = telemetry[field].clone();
                        }
                    }
                }
                if !telemetry["native_session_id"].is_null() {
                    worker["native_session_id"] = telemetry["native_session_id"].clone();
                }
                if telemetry["usage"]
                    .as_object()
                    .is_some_and(|usage| !usage.is_empty())
                {
                    worker["usage"] = telemetry["usage"].clone();
                }
            }
        }
        if active && worker["status"] == "running" && attempt_accepts_control {
            if capabilities
                .iter()
                .any(|capability| capability == attachment::WORKER_STEERING)
                && worker_configuration_supports_control(
                    &worker["configuration"],
                    WorkerControlAction::Steer,
                )
            {
                controls.push(Value::String("steer".into()));
            }
            if capabilities
                .iter()
                .any(|capability| capability == attachment::WORKER_INTERRUPTION)
                && worker_configuration_supports_control(
                    &worker["configuration"],
                    WorkerControlAction::Interrupt,
                )
            {
                controls.push(Value::String("interrupt".into()));
            }
        }
        let retry_available = if active
            && worker["status"] == "interrupted"
            && capabilities
                .iter()
                .any(|capability| capability == attachment::WORKER_INTERRUPTION)
        {
            worker["assignment"]["id"]
                .as_str()
                .map(|assignment_id| {
                    worker_retry_available(connection, commission_id, assignment_id)
                })
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        if retry_available {
            controls.push(Value::String("retry".into()));
        }
        worker["available_controls"] = Value::Array(controls);
    }
    Ok(())
}

fn worker_configuration_supports_control(
    configuration: &Value,
    action: WorkerControlAction,
) -> bool {
    let structured_adapter = matches!(
        configuration["adapter"]["kind"].as_str(),
        Some("codex_app_server" | "claude_agent_sdk")
    );
    if !structured_adapter {
        return false;
    }
    match action {
        WorkerControlAction::Steer => true,
        WorkerControlAction::Interrupt => {
            configuration["capabilities"]
                .as_array()
                .is_some_and(|capabilities| {
                    capabilities
                        .iter()
                        .any(|capability| capability == "semantic_interrupt")
                })
        }
    }
}

fn worker_retry_available(
    connection: &Connection,
    commission_id: &str,
    assignment_id: &str,
) -> Result<bool, TyrionError> {
    Ok(connection.query_row(
        "SELECT
            EXISTS (
                SELECT 1 FROM attention_conditions
                WHERE commission_id = ?1 AND assignment_id = ?2
                  AND code = 'worker_interrupted' AND status = 'open'
            )
            AND (
                SELECT COUNT(*) FROM attempts
                JOIN assignments ON assignments.id = attempts.assignment_id
                WHERE assignments.commission_id = ?1
                  AND NOT EXISTS (
                      SELECT 1 FROM worker_configuration_failures
                      WHERE worker_configuration_failures.attempt_id = attempts.id
                  )
            ) < resource_ceilings.max_attempts
         FROM resource_ceilings WHERE commission_id = ?1",
        params![commission_id, assignment_id],
        |row| row.get::<_, bool>(0),
    )?)
}

fn replay_events(
    connection: &Connection,
    attachment_id: &str,
    commission_id: &str,
    after_sequence: i64,
) -> Result<Value, TyrionError> {
    let (role, mode, capabilities_json, missing_capabilities_json) = connection.query_row(
        "SELECT commission_attachments.role, attachments.mode,
                attachments.capabilities_json, attachments.missing_capabilities_json
         FROM commission_attachments
         JOIN attachments ON attachments.id = commission_attachments.attachment_id
         WHERE commission_attachments.commission_id = ?1
           AND commission_attachments.attachment_id = ?2",
        params![commission_id, attachment_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    )?;
    let capabilities = serde_json::from_str::<Vec<String>>(&capabilities_json)?;
    let missing_capabilities = serde_json::from_str::<Vec<Value>>(&missing_capabilities_json)?;
    let mut statement = connection.prepare(
        "SELECT sequence, event_type, commission_revision, payload_json, created_at
         FROM events
         WHERE commission_id = ?1 AND sequence > ?2
         ORDER BY sequence",
    )?;
    let rows = statement.query_map(params![commission_id, after_sequence], event_value)?;
    let events = rows.collect::<Result<Vec<_>, _>>()?;
    let next_event_sequence = events
        .last()
        .and_then(|event| event["sequence"].as_i64())
        .unwrap_or(after_sequence);
    let may_receive_material_notifications = role == "active"
        && capabilities
            .iter()
            .any(|capability| capability == attachment::MATERIAL_NOTIFICATIONS);
    let material_notifications = if may_receive_material_notifications {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event["type"].as_str(),
                    Some(
                        "commission_verified_complete"
                            | "assignment_blocked"
                            | "operation_notification"
                            | "approval_gate_opened"
                            | "operation_confirmed"
                            | "operation_failed"
                            | "operation_uncertain"
                            | "commission_amendment_proposed"
                            | "resource_ceiling_approaching"
                    )
                )
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mode = attachment::AttachmentMode::parse(&mode)?;

    Ok(serde_json::json!({
        "attachment_id": attachment_id,
        "attachment_mode": mode.as_str(),
        "mode_tag": mode.tag(),
        "commission_id": commission_id,
        "commission_role": role,
        "missing_capabilities": missing_capabilities,
        "events": events,
        "next_event_sequence": next_event_sequence,
        "material_notifications": material_notifications,
    }))
}

fn validate_proposal(proposal: &CommissionProposal) -> Result<(), TyrionError> {
    if proposal.goal.trim().is_empty() {
        return Err(TyrionError::InvalidRequest("goal must not be empty".into()));
    }
    if proposal.project_id.as_ref().is_some_and(|project_id| {
        project_id.trim() != project_id || project_id.is_empty() || project_id.contains('\0')
    }) {
        return Err(TyrionError::InvalidRequest(
            "project_id must be a non-empty trimmed identifier".into(),
        ));
    }
    if proposal.project_id.is_some() && proposal.authority.repositories.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "a project-scoped Commission must name repository Evidence for identity verification"
                .into(),
        ));
    }
    if proposal.commission_constraints.iter().any(|constraint| {
        constraint.trim() != constraint || constraint.is_empty() || constraint.contains('\0')
    }) {
        return Err(TyrionError::InvalidRequest(
            "Commission constraints must be non-empty trimmed statements".into(),
        ));
    }
    validate_worker_requirements(
        &proposal.worker_requirements,
        SkillSelectionProvenance::Principal,
    )?;
    validate_acceptance_criteria(&proposal.execution, &proposal.criteria)?;
    if proposal.resource_ceilings.max_attempts == 0
        || proposal.resource_ceilings.max_elapsed_seconds == 0
        || proposal.resource_ceilings.max_worker_concurrency == 0
        || proposal.resource_ceilings.max_storage_bytes == 0
    {
        return Err(TyrionError::InvalidRequest(
            "attempt, elapsed-time, concurrency, and storage ceilings must be positive".into(),
        ));
    }
    match &proposal.execution {
        ExecutionSpec::Deterministic => ensure_result_fits_storage_ceiling(
            &proposal.goal,
            proposal.resource_ceilings.max_storage_bytes,
        )?,
        ExecutionSpec::CodexGit {
            repository,
            base_revision,
        } => {
            if !Path::new(repository).is_absolute() {
                return Err(TyrionError::InvalidRequest(
                    "codex_git repository must be an absolute path".into(),
                ));
            }
            Path::new(repository).canonicalize().map_err(|error| {
                TyrionError::InvalidRequest(format!(
                    "codex_git repository cannot be resolved: {error}"
                ))
            })?;
            if !proposal
                .authority
                .repositories
                .iter()
                .any(|value| value == repository)
            {
                return Err(TyrionError::InvalidRequest(
                    "codex_git repository must be named in the Authority Envelope".into(),
                ));
            }
            if !proposal
                .authority
                .actions
                .iter()
                .any(|action| action == worker::CODEX_GIT_ACTION)
            {
                return Err(TyrionError::InvalidRequest(
                    "the Authority Envelope does not permit codex.git_change".into(),
                ));
            }
            if proposal.authority.paths.is_empty() {
                return Err(TyrionError::InvalidRequest(
                    "codex_git execution requires at least one authorized changed path".into(),
                ));
            }
            for path in &proposal.authority.paths {
                let path = Path::new(path);
                if path.is_absolute()
                    || path
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    return Err(TyrionError::InvalidRequest(
                        "authorized changed paths must be normalized relative paths".into(),
                    ));
                }
            }
            if !(40..=64).contains(&base_revision.len())
                || !base_revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(TyrionError::InvalidRequest(
                    "codex_git base_revision must be a full hexadecimal Git object id".into(),
                ));
            }
            if !proposal.authority.destinations.is_empty() || !proposal.authority.effects.is_empty()
            {
                return Err(TyrionError::InvalidRequest(
                    "the contained codex_git slice does not permit external effects".into(),
                ));
            }
        }
    }
    let sqlite_integer_max = i64::MAX as u64;
    if proposal.resource_ceilings.max_elapsed_seconds > sqlite_integer_max
        || proposal.resource_ceilings.max_storage_bytes > sqlite_integer_max
        || proposal.resource_ceilings.max_model_spend_cents > sqlite_integer_max
        || proposal.resource_ceilings.max_paid_service_spend_cents > sqlite_integer_max
    {
        return Err(TyrionError::InvalidRequest(
            "resource ceilings must fit in a signed 64-bit integer".into(),
        ));
    }
    if let Some(plan) = &proposal.plan {
        validate_commission_plan(proposal, plan)?;
    }
    Ok(())
}

fn bind_project_identity(
    transaction: &Transaction<'_>,
    proposal: &CommissionProposal,
) -> Result<(), TyrionError> {
    let Some(project_id) = proposal.project_id.as_deref() else {
        return Ok(());
    };
    let project_exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE project_id = ?1)",
        [project_id],
        |row| row.get::<_, bool>(0),
    )?;
    let mut verified_anchor = false;
    let mut repositories = Vec::new();
    for repository in &proposal.authority.repositories {
        let canonical_repository = fs::canonicalize(repository).map_err(|error| {
            TyrionError::InvalidRequest(format!(
                "project repository Evidence cannot be resolved: {error}"
            ))
        })?;
        if !canonical_repository.is_dir() {
            return Err(TyrionError::InvalidRequest(
                "project repository Evidence must identify a directory".into(),
            ));
        }
        let metadata = fs::metadata(&canonical_repository)?;
        let repository_device = i64::try_from(metadata.dev()).map_err(|_| {
            TyrionError::InvalidRequest("project repository device identity is out of range".into())
        })?;
        let repository_inode = i64::try_from(metadata.ino()).map_err(|_| {
            TyrionError::InvalidRequest("project repository inode identity is out of range".into())
        })?;
        let bound_project = transaction
            .query_row(
                "SELECT project_id FROM project_identities
                 WHERE repository_device = ?1 AND repository_inode = ?2",
                params![repository_device, repository_inode],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match bound_project.as_deref() {
            Some(bound_project) if bound_project != project_id => {
                return Err(TyrionError::InvalidRequest(format!(
                    "repository identity is already bound to Project {bound_project}"
                )));
            }
            Some(_) => verified_anchor = true,
            None => {}
        }
        repositories.push((
            path_string(&canonical_repository)?,
            repository_device,
            repository_inode,
        ));
    }
    if project_exists && !verified_anchor {
        return Err(TyrionError::InvalidRequest(format!(
            "Project {project_id} is bound to a different repository identity set"
        )));
    }
    let now = unix_timestamp()?;
    transaction.execute(
        "INSERT OR IGNORE INTO projects (project_id, created_at) VALUES (?1, ?2)",
        params![project_id, now],
    )?;
    for (canonical_repository, repository_device, repository_inode) in repositories {
        transaction.execute(
            "INSERT OR IGNORE INTO project_identities (
                project_id, repository_device, repository_inode, created_at
             ) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, repository_device, repository_inode, now],
        )?;
        transaction.execute(
            "INSERT INTO project_aliases (
                project_id, repository_device, repository_inode,
                canonical_repository, observed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_id, canonical_repository)
             DO UPDATE SET observed_at = excluded.observed_at",
            params![
                project_id,
                repository_device,
                repository_inode,
                canonical_repository,
                now
            ],
        )?;
    }
    Ok(())
}

fn validate_commission_plan(
    proposal: &CommissionProposal,
    plan: &CommissionPlan,
) -> Result<(), TyrionError> {
    if plan.assignments.len() < 2 {
        return Err(TyrionError::InvalidRequest(
            "an explicit Commission Plan must contain at least two Assignments".into(),
        ));
    }
    if plan.assignments.len() as u32 > proposal.resource_ceilings.max_attempts {
        return Err(TyrionError::InvalidRequest(
            "max_attempts must cover every Assignment in the initial Commission Plan".into(),
        ));
    }
    if proposal
        .criteria
        .iter()
        .any(|criterion| criterion.verifier_type != VerifierType::Deterministic)
    {
        return Err(TyrionError::InvalidRequest(
            "multi-Assignment plans currently require deterministic criterion Evidence".into(),
        ));
    }

    let criterion_ids = proposal
        .criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect::<HashSet<_>>();
    let mut assignment_ids = HashSet::new();
    let mut owned_criteria = HashSet::new();
    let mut competitions: HashMap<&str, (&str, &str, Vec<Resources>)> = HashMap::new();
    let mut competition_assignments: HashMap<&str, Vec<&PlannedAssignment>> = HashMap::new();
    let mut cumulative_model_spend = 0_u64;
    let mut cumulative_paid_spend = 0_u64;
    for assignment in &plan.assignments {
        if assignment.id.trim().is_empty() || assignment.goal.trim().is_empty() {
            return Err(TyrionError::InvalidRequest(
                "planned Assignment id and goal must not be empty".into(),
            ));
        }
        if !assignment_ids.insert(assignment.id.as_str()) {
            return Err(TyrionError::InvalidRequest(format!(
                "planned Assignment id {} is duplicated",
                assignment.id
            )));
        }
        if assignment.criterion_ids.is_empty() {
            return Err(TyrionError::InvalidRequest(format!(
                "planned Assignment {} must own at least one Acceptance Criterion",
                assignment.id
            )));
        }
        validate_worker_requirements(
            &assignment.worker_requirements,
            SkillSelectionProvenance::Plan,
        )?;
        merge_worker_requirements(
            &proposal.worker_requirements,
            &assignment.worker_requirements,
        )?;
        for criterion_id in &assignment.criterion_ids {
            if !criterion_ids.contains(criterion_id.as_str()) {
                return Err(TyrionError::InvalidRequest(format!(
                    "planned Assignment {} names unknown criterion {}",
                    assignment.id, criterion_id
                )));
            }
            if !owned_criteria.insert(criterion_id.as_str()) {
                return Err(TyrionError::InvalidRequest(format!(
                    "Acceptance Criterion {criterion_id} is owned by more than one planned Assignment"
                )));
            }
        }
        validate_assignment_resources(
            &assignment.id,
            &assignment.resources,
            &proposal.resource_ceilings,
        )?;
        cumulative_model_spend = cumulative_model_spend
            .checked_add(assignment.resources.max_model_spend_cents)
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "planned cumulative model spend exceeds the Commission ceiling".into(),
                )
            })?;
        cumulative_paid_spend = cumulative_paid_spend
            .checked_add(assignment.resources.max_paid_service_spend_cents)
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "planned cumulative paid-service spend exceeds the Commission ceiling".into(),
                )
            })?;
        for scope in assignment
            .read_scopes
            .iter()
            .chain(assignment.write_scopes.iter())
        {
            validate_relative_scope(scope)?;
        }
        for scope in &assignment.write_scopes {
            if !proposal
                .authority
                .paths
                .iter()
                .any(|authorized| path_is_within_scope(scope, authorized))
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "planned Assignment {} declares unauthorized write scope {}",
                    assignment.id, scope
                )));
            }
        }
        if let Some(competition) = &assignment.competition {
            competition_assignments
                .entry(&competition.group)
                .or_default()
                .push(assignment);
            if competition.group.trim().is_empty()
                || competition.uncertainty.trim().is_empty()
                || competition.comparison_rule.trim().is_empty()
            {
                return Err(TyrionError::InvalidRequest(
                    "competing work requires a group, uncertainty, and comparison rule".into(),
                ));
            }
            let entry = competitions.entry(&competition.group).or_insert((
                &competition.uncertainty,
                &competition.comparison_rule,
                Vec::new(),
            ));
            if entry.0 != competition.uncertainty || entry.1 != competition.comparison_rule {
                return Err(TyrionError::InvalidRequest(format!(
                    "competition group {} must use one uncertainty and comparison rule",
                    competition.group
                )));
            }
            entry.2.push(Resources {
                concurrency: assignment.resources.concurrency_slots.into(),
                storage: assignment.resources.max_storage_bytes,
                model_spend: assignment.resources.max_model_spend_cents,
                paid_spend: assignment.resources.max_paid_service_spend_cents,
            });
        }
    }
    let comparison_requirements = competitions
        .iter()
        .map(|(group, (_, _, member_resources))| {
            Ok((
                *group,
                member_resources.len(),
                comparison_resources(member_resources.iter().copied())?,
            ))
        })
        .collect::<Result<Vec<_>, TyrionError>>()?;
    let competition_attempts = comparison_requirements.len() as u32;
    if (plan.assignments.len() as u32)
        .checked_add(competition_attempts)
        .is_none_or(|required| required > proposal.resource_ceilings.max_attempts)
    {
        return Err(TyrionError::InvalidRequest(
            "max_attempts must include one comparison Assignment per competition group".into(),
        ));
    }
    let reconciliation_model_spend = comparison_requirements
        .iter()
        .try_fold(0_u64, |total, (_, _, resources)| {
            total.checked_add(resources.model_spend)
        });
    let reconciliation_paid_spend = comparison_requirements
        .iter()
        .try_fold(0_u64, |total, (_, _, resources)| {
            total.checked_add(resources.paid_spend)
        });
    cumulative_model_spend = cumulative_model_spend
        .checked_add(reconciliation_model_spend.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "planned cumulative model spend exceeds the Commission ceiling".into(),
            )
        })?)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "planned cumulative model spend exceeds the Commission ceiling".into(),
            )
        })?;
    cumulative_paid_spend = cumulative_paid_spend
        .checked_add(reconciliation_paid_spend.ok_or_else(|| {
            TyrionError::InvalidRequest(
                "planned cumulative paid-service spend exceeds the Commission ceiling".into(),
            )
        })?)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "planned cumulative paid-service spend exceeds the Commission ceiling".into(),
            )
        })?;
    if cumulative_model_spend > proposal.resource_ceilings.max_model_spend_cents {
        return Err(TyrionError::InvalidRequest(
            "planned cumulative model spend exceeds the Commission ceiling".into(),
        ));
    }
    if cumulative_paid_spend > proposal.resource_ceilings.max_paid_service_spend_cents {
        return Err(TyrionError::InvalidRequest(
            "planned cumulative paid-service spend exceeds the Commission ceiling".into(),
        ));
    }
    if comparison_requirements
        .iter()
        .any(|(_, _, resources)| resources.storage > proposal.resource_ceilings.max_storage_bytes)
    {
        return Err(TyrionError::InvalidRequest(
            "each competition comparison working set must fit the Commission storage ceiling"
                .into(),
        ));
    }
    if owned_criteria != criterion_ids {
        return Err(TyrionError::InvalidRequest(
            "every Acceptance Criterion must be owned by exactly one planned Assignment".into(),
        ));
    }
    for assignment in &plan.assignments {
        for dependency in &assignment.dependencies {
            if dependency == &assignment.id || !assignment_ids.contains(dependency.as_str()) {
                return Err(TyrionError::InvalidRequest(format!(
                    "planned Assignment {} has invalid dependency {}",
                    assignment.id, dependency
                )));
            }
        }
    }
    for (group, members) in competition_assignments {
        let member_ids = members
            .iter()
            .map(|assignment| assignment.id.as_str())
            .collect::<HashSet<_>>();
        let expected_dependencies = members[0]
            .dependencies
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for member in members {
            if member
                .dependencies
                .iter()
                .any(|dependency| member_ids.contains(dependency.as_str()))
                || member
                    .dependencies
                    .iter()
                    .map(String::as_str)
                    .collect::<HashSet<_>>()
                    != expected_dependencies
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "competition group {group} members must share one dependency frontier and cannot depend on each other"
                )));
            }
        }
    }
    for (group, members, _) in comparison_requirements {
        if members < 2 {
            return Err(TyrionError::InvalidRequest(format!(
                "competition group {group} must contain at least two planned Assignments"
            )));
        }
    }
    ensure_acyclic_plan(plan)?;
    Ok(())
}

fn validate_worker_requirements(
    requirements: &WorkerRequirements,
    expected_selection_provenance: SkillSelectionProvenance,
) -> Result<(), TyrionError> {
    let named_sets = [
        ("capability", &requirements.capabilities),
        ("tool", &requirements.tools),
        (
            "Assignment constraint",
            &requirements.assignment_constraints,
        ),
        (
            "required Worker Configuration",
            &requirements.require_configurations,
        ),
        (
            "excluded Worker Configuration",
            &requirements.exclude_configurations,
        ),
    ];
    for (name, values) in named_sets {
        let mut unique = HashSet::new();
        for value in values {
            if value.trim().is_empty() || value.contains('\0') {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker requirement {name} names must not be empty"
                )));
            }
            if !unique.insert(value) {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker requirement {name} {value} is duplicated"
                )));
            }
        }
    }
    let mut skill_names = HashSet::new();
    for skill in &requirements.skills {
        if !skill.is_content_identified() {
            return Err(TyrionError::InvalidRequest(format!(
                "Required Skill Version {} requires a lowercase sha256 content digest",
                skill.name
            )));
        }
        if !skill_names.insert(skill.name.as_str()) {
            return Err(TyrionError::InvalidRequest(format!(
                "Required Skill Version {} is duplicated",
                skill.name
            )));
        }
    }
    let mut selected_names = HashSet::new();
    for selected in &requirements.selected_skills {
        let version = selected.version();
        if !version.is_content_identified() {
            return Err(TyrionError::InvalidRequest(format!(
                "Selected Skill Version {} requires a lowercase sha256 content digest",
                selected.name
            )));
        }
        if selected.provenance != expected_selection_provenance {
            return Err(TyrionError::InvalidRequest(format!(
                "Selected Skill Version {} must use {} provenance in this constraint",
                selected.name,
                expected_selection_provenance.as_str()
            )));
        }
        if !selected_names.insert(selected.name.as_str())
            || skill_names.contains(selected.name.as_str())
        {
            return Err(TyrionError::InvalidRequest(format!(
                "Skill Version {} cannot be selected more than once or also be required",
                selected.name
            )));
        }
    }
    if requirements
        .context_strategy
        .as_deref()
        .is_some_and(|strategy| strategy.trim().is_empty() || strategy.contains('\0'))
    {
        return Err(TyrionError::InvalidRequest(
            "Worker requirement context strategy must not be empty".into(),
        ));
    }
    if requirements
        .require_configurations
        .iter()
        .any(|required| requirements.exclude_configurations.contains(required))
    {
        return Err(TyrionError::InvalidRequest(
            "a Worker Configuration cannot be both required and excluded".into(),
        ));
    }
    Ok(())
}

fn merge_worker_requirements(
    principal: &WorkerRequirements,
    planned: &WorkerRequirements,
) -> Result<WorkerRequirements, TyrionError> {
    let mut skills = principal.skills.clone();
    for skill in &planned.skills {
        if let Some(existing) = skills.iter().find(|existing| existing.name == skill.name) {
            if existing != skill {
                return Err(TyrionError::InvalidRequest(format!(
                    "Required Skill Version {} conflicts between Principal and plan constraints",
                    skill.name
                )));
            }
        } else {
            skills.push(skill.clone());
        }
    }
    let mut selected_skills = principal.selected_skills.clone();
    for selected in &planned.selected_skills {
        if let Some(existing) = selected_skills
            .iter()
            .find(|existing| existing.name == selected.name)
        {
            if existing.content_digest != selected.content_digest {
                return Err(TyrionError::InvalidRequest(format!(
                    "Selected Skill Version {} conflicts between Principal and plan constraints",
                    selected.name
                )));
            }
        } else {
            selected_skills.push(selected.clone());
        }
    }
    for skill in &skills {
        if selected_skills
            .iter()
            .any(|selected| selected.name == skill.name)
        {
            return Err(TyrionError::InvalidRequest(format!(
                "Skill Version {} cannot be both required and optionally selected",
                skill.name
            )));
        }
    }
    let mut merged = planned.clone();
    merged.skills = skills;
    merged.selected_skills = selected_skills;
    Ok(merged)
}

fn comparison_resources(
    members: impl IntoIterator<Item = Resources>,
) -> Result<Resources, TyrionError> {
    let mut comparison = Resources::default();
    let mut member_storage = 0_u64;
    for member in members {
        comparison.concurrency = comparison.concurrency.max(member.concurrency);
        comparison.storage = comparison.storage.max(member.storage);
        comparison.model_spend = comparison.model_spend.max(member.model_spend);
        comparison.paid_spend = comparison.paid_spend.max(member.paid_spend);
        member_storage = member_storage.checked_add(member.storage).ok_or_else(|| {
            TyrionError::InvalidRequest("competition comparison storage budget overflows".into())
        })?;
    }
    comparison.storage = member_storage
        .checked_add(comparison.storage)
        .ok_or_else(|| {
            TyrionError::InvalidRequest("competition comparison storage budget overflows".into())
        })?;
    Ok(comparison)
}

fn validate_assignment_resources(
    assignment_id: &str,
    resources: &AssignmentResources,
    ceilings: &ResourceCeilings,
) -> Result<(), TyrionError> {
    if resources.concurrency_slots == 0 || resources.max_storage_bytes == 0 {
        return Err(TyrionError::InvalidRequest(format!(
            "planned Assignment {assignment_id} requires positive concurrency and storage reservations"
        )));
    }
    if resources.concurrency_slots > ceilings.max_worker_concurrency
        || resources.max_storage_bytes > ceilings.max_storage_bytes
        || resources.max_model_spend_cents > ceilings.max_model_spend_cents
        || resources.max_paid_service_spend_cents > ceilings.max_paid_service_spend_cents
    {
        return Err(TyrionError::InvalidRequest(format!(
            "planned Assignment {assignment_id} requests resources outside the Commission ceilings"
        )));
    }
    Ok(())
}

fn validate_relative_scope(scope: &str) -> Result<(), TyrionError> {
    let path = Path::new(scope);
    if scope.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(TyrionError::InvalidRequest(
            "planned artifact scopes must be normalized relative paths".into(),
        ));
    }
    Ok(())
}

fn path_is_within_scope(path: &str, scope: &str) -> bool {
    path == scope || path.starts_with(&format!("{scope}/"))
}

fn validate_operation_shape(operation: &OperationRequest) -> Result<(), TyrionError> {
    if operation.operation.trim().is_empty()
        || operation.assignment_id.trim().is_empty()
        || operation.attempt_id.trim().is_empty()
        || operation.worker_lease_id.trim().is_empty()
        || operation.consequences.is_empty()
        || operation
            .consequences
            .iter()
            .any(|consequence| consequence.trim().is_empty())
        || operation.limits.max_output_bytes == 0
        || operation.limits.max_duration_seconds == 0
    {
        return Err(TyrionError::InvalidRequest(
            "operation identity, consequences, and positive limits are required".into(),
        ));
    }
    if operation.operation.starts_with("credential.") != operation.credential.is_some() {
        return Err(TyrionError::InvalidRequest(
            "credentialed operations require one explicit Credential Grant use".into(),
        ));
    }
    validate_relative_scope(&operation.target)?;
    if operation
        .parameters
        .iter()
        .any(|(name, value)| name.trim().is_empty() || name.contains('\0') || value.contains('\0'))
        || operation
            .repository
            .as_deref()
            .is_some_and(|repository| repository.trim().is_empty() || repository.contains('\0'))
        || operation
            .destination
            .as_deref()
            .is_some_and(|destination| destination.trim().is_empty() || destination.contains('\0'))
        || operation
            .effect
            .as_deref()
            .is_some_and(|effect| effect.trim().is_empty() || effect.contains('\0'))
    {
        return Err(TyrionError::InvalidRequest(
            "operation fields must not be empty or contain NUL bytes".into(),
        ));
    }
    Ok(())
}

fn canonical_operation(
    operation: &OperationRequest,
    credential_runtime: Option<&CredentialRuntime>,
) -> Result<(Value, Option<Value>), TyrionError> {
    let mut canonical = serde_json::to_value(operation)?;
    let binding = if operation.operation == "filesystem.write"
        && operation.effect.as_deref() == Some("filesystem.write")
        && operation.destination.as_deref() == Some("local")
    {
        Some(serde_json::to_value(bind_file_effect(operation)?)?)
    } else if matches!(
        operation.operation.as_str(),
        "credential.http.request" | "credential.command.request"
    ) && operation.effect.as_deref() == Some("external.write")
    {
        let runtime = credential_runtime.ok_or_else(|| {
            TyrionError::ControlDenied("credential brokering is not configured".into())
        })?;
        Some(serde_json::to_value(runtime.bind(operation)?)?)
    } else {
        None
    };
    if let Some(binding) = &binding {
        canonical
            .as_object_mut()
            .ok_or_else(|| {
                TyrionError::InvalidRequest("canonical operation must be an object".into())
            })?
            .insert("target_revision".into(), serde_json::to_value(binding)?);
    }
    Ok((canonical, binding))
}

fn canonical_operation_with_binding(
    operation: &OperationRequest,
    binding: &Value,
) -> Result<Value, TyrionError> {
    let mut canonical = serde_json::to_value(operation)?;
    canonical
        .as_object_mut()
        .ok_or_else(|| TyrionError::InvalidRequest("canonical operation must be an object".into()))?
        .insert("target_revision".into(), binding.clone());
    Ok(canonical)
}

fn bind_file_effect(operation: &OperationRequest) -> Result<FileEffectBinding, TyrionError> {
    let repository = operation.repository.as_deref().ok_or_else(|| {
        TyrionError::InvalidRequest("filesystem.write requires an exact repository".into())
    })?;
    let canonical_repository = fs::canonicalize(repository)?;
    if !canonical_repository.is_dir() {
        return Err(TyrionError::InvalidRequest(
            "the approved repository is not a directory".into(),
        ));
    }
    let target = canonical_repository.join(&operation.target);
    let parent = target.parent().ok_or_else(|| {
        TyrionError::InvalidRequest("the approved target has no parent directory".into())
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    if !canonical_parent.starts_with(&canonical_repository) {
        return Err(TyrionError::ControlDenied(
            "the approved target escapes its exact repository".into(),
        ));
    }
    let target_name = target.file_name().ok_or_else(|| {
        TyrionError::InvalidRequest("the approved target has no file name".into())
    })?;
    let directory = fs::File::from(
        openat(
            CWD,
            &canonical_parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let mut target_file = fs::File::from(
        openat(
            &directory,
            target_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let repository_metadata = fs::metadata(&canonical_repository)?;
    let parent_metadata = directory.metadata()?;
    let target_metadata = target_file.metadata()?;
    if !target_metadata.is_file() {
        return Err(TyrionError::ControlDenied(
            "filesystem.write requires an existing regular non-symlink target".into(),
        ));
    }
    let before_sha256 = sha256_reader(&mut target_file)?;
    Ok(FileEffectBinding {
        canonical_repository: path_string(&canonical_repository)?,
        canonical_parent: path_string(&canonical_parent)?,
        repository_device: repository_metadata.dev(),
        repository_inode: repository_metadata.ino(),
        parent_device: parent_metadata.dev(),
        parent_inode: parent_metadata.ino(),
        target_device: target_metadata.dev(),
        target_inode: target_metadata.ino(),
        before_sha256,
    })
}

fn observe_effect_target(
    operation: &OperationRequest,
    binding: &FileEffectBinding,
) -> Result<String, TyrionError> {
    let repository_metadata = fs::metadata(&binding.canonical_repository)?;
    if repository_metadata.dev() != binding.repository_device
        || repository_metadata.ino() != binding.repository_inode
    {
        return Err(TyrionError::ControlDenied(
            "the reconciled repository is not the exact approved repository".into(),
        ));
    }
    let directory = fs::File::from(
        openat(
            CWD,
            binding.canonical_parent.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let directory_metadata = directory.metadata()?;
    if directory_metadata.dev() != binding.parent_device
        || directory_metadata.ino() != binding.parent_inode
    {
        return Err(TyrionError::ControlDenied(
            "the reconciled directory is not the exact approved directory".into(),
        ));
    }
    let target_name = Path::new(&operation.target)
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("effect target has no file name".into()))?;
    let mut target = fs::File::from(
        openat(
            &directory,
            target_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    if !target.metadata()?.is_file() {
        return Err(TyrionError::ControlDenied(
            "Principal reconciliation requires an exact regular non-symlink target".into(),
        ));
    }
    sha256_reader(&mut target)
}

fn ensure_file_effect_binding(
    operation: &OperationRequest,
    binding: &FileEffectBinding,
) -> Result<(), TyrionError> {
    let requested_repository = operation.repository.as_deref().ok_or_else(|| {
        TyrionError::InvalidRequest("filesystem.write requires an exact repository".into())
    })?;
    if fs::canonicalize(requested_repository)? != Path::new(&binding.canonical_repository) {
        return Err(TyrionError::ControlDenied(
            "the exact approved repository path resolves to a different repository".into(),
        ));
    }
    let repository_metadata = fs::metadata(&binding.canonical_repository)?;
    if repository_metadata.dev() != binding.repository_device
        || repository_metadata.ino() != binding.repository_inode
    {
        return Err(TyrionError::ControlDenied(
            "the approved repository identity changed before execution".into(),
        ));
    }
    let directory = fs::File::from(
        openat(
            CWD,
            binding.canonical_parent.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let directory_metadata = directory.metadata()?;
    if directory_metadata.dev() != binding.parent_device
        || directory_metadata.ino() != binding.parent_inode
    {
        return Err(TyrionError::ControlDenied(
            "the approved target directory identity changed before execution".into(),
        ));
    }
    let target_name = Path::new(&operation.target)
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("effect target has no file name".into()))?;
    ensure_bound_target(&directory, target_name, binding)?;
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), TyrionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TyrionError::InvalidRequest(
            "observed_sha256 must be an exact 64-character hexadecimal digest".into(),
        ));
    }
    Ok(())
}

fn path_string(path: &Path) -> Result<String, TyrionError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| TyrionError::InvalidRequest("effect paths must contain valid UTF-8".into()))
}

fn sha256_reader(reader: &mut impl Read) -> Result<String, TyrionError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn classify_operation(
    operation: &OperationRequest,
    authority: &AuthorityEnvelope,
) -> (OperationClassification, &'static str) {
    let repository_scoped = matches!(
        operation.operation.as_str(),
        "repository.read" | "repository.edit" | "filesystem.write"
    );
    let repository_authorized = !repository_scoped
        || operation.repository.as_ref().is_some_and(|repository| {
            authority
                .repositories
                .iter()
                .any(|authorized| authorized == repository)
        });
    let target_authorized = !repository_scoped
        || authority
            .paths
            .iter()
            .any(|authorized| path_is_within_scope(&operation.target, authorized));
    let action_authorized = authority
        .actions
        .iter()
        .any(|authorized| authorized == &operation.operation);
    let destination_authorized = operation.destination.as_ref().is_none_or(|destination| {
        authority
            .destinations
            .iter()
            .any(|authorized| authorized == destination)
    });
    let effect_authorized = operation.effect.as_ref().is_none_or(|effect| {
        authority
            .effects
            .iter()
            .any(|authorized| authorized == effect)
    });
    if !repository_authorized
        || !target_authorized
        || !action_authorized
        || !destination_authorized
        || !effect_authorized
    {
        return (
            OperationClassification::Prohibited,
            "the exact repository, target, action, destination, or effect is outside the current Authority Envelope",
        );
    }
    match operation.operation.as_str() {
        "repository.read" if operation.destination.is_none() && operation.effect.is_none() => (
            OperationClassification::SilentJournaled,
            "authorized read-only repository work is routine and journaled",
        ),
        "repository.edit" if operation.destination.is_none() && operation.effect.is_none() => (
            OperationClassification::NonBlockingNotification,
            "authorized reversible repository edits remain visible without blocking work",
        ),
        "filesystem.write"
            if operation.destination.as_deref() == Some("local")
                && operation.effect.as_deref() == Some("filesystem.write") =>
        {
            (
                OperationClassification::ApprovalGate,
                "the requested file replacement is a consequential effect requiring exact Principal approval",
            )
        }
        "credential.http.request"
            if operation.repository.is_none()
                && operation.destination.is_some()
                && operation.effect.as_deref() == Some("external.write")
                && operation.credential.is_some() =>
        {
            (
                OperationClassification::ApprovalGate,
                "the credentialed external request requires exact Principal approval",
            )
        }
        "credential.command.request"
            if operation.repository.is_none()
                && operation.destination.is_some()
                && operation.effect.as_deref() == Some("external.write")
                && operation.credential.as_ref().is_some_and(|credential| {
                    credential.mode == CredentialUseMode::OneShotExposure
                }) =>
        {
            (
                OperationClassification::ApprovalGate,
                "the exceptional one-shot credential exposure requires exact Principal approval",
            )
        }
        _ => (
            OperationClassification::Prohibited,
            "the canonical operation is unsupported or its effect fields are inconsistent",
        ),
    }
}

fn ensure_current_operation_context(
    connection: &Connection,
    commission_id: &str,
    operation: &OperationRequest,
    expected_revision: i64,
) -> Result<(), TyrionError> {
    let context = connection
        .query_row(
            "SELECT commissions.status, commissions.revision, assignments.plan_revision,
                    attempts.status, worker_leases.status, worker_leases.expires_at,
                    worker_leases.mandate_revision
             FROM commissions
             JOIN assignments ON assignments.commission_id = commissions.id
             JOIN attempts ON attempts.assignment_id = assignments.id
             JOIN worker_leases ON worker_leases.attempt_id = attempts.id
             WHERE commissions.id = ?1 AND assignments.id = ?2
               AND attempts.id = ?3 AND worker_leases.id = ?4",
            params![
                commission_id,
                operation.assignment_id,
                operation.attempt_id,
                operation.worker_lease_id,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::ControlDenied(
                "operation does not identify a current Assignment, Attempt, and Worker Lease"
                    .into(),
            )
        })?;
    let (
        commission_status,
        current_revision,
        plan_revision,
        attempt_status,
        lease_status,
        expires_at,
        lease_mandate_revision,
    ) = context;
    if current_revision != expected_revision {
        return Err(TyrionError::StaleRevision {
            expected: expected_revision,
            actual: current_revision,
        });
    }
    if commission_status != CommissionStatus::Active.as_str()
        || operation.mandate_revision != current_revision
        || operation.plan_revision != plan_revision
        || attempt_status != AttemptStatus::Running.as_str()
        || lease_status != WorkerLeaseStatus::Active.as_str()
        || lease_mandate_revision != current_revision
        || expires_at <= unix_timestamp()?
    {
        return Err(TyrionError::ControlDenied(
            "operation authority is stale or no longer active".into(),
        ));
    }
    Ok(())
}

fn validate_credential_grant_shape(grant: &CredentialGrantRequest) -> Result<(), TyrionError> {
    if grant.assignment_id.trim().is_empty()
        || grant.attempt_id.trim().is_empty()
        || grant.worker_lease_id.trim().is_empty()
        || grant.credential_reference.trim().is_empty()
        || grant.capability.trim().is_empty()
        || grant.destination.trim().is_empty()
        || grant.credential_reference.contains('\0')
        || grant.capability.contains('\0')
        || grant.destination.contains('\0')
    {
        return Err(TyrionError::InvalidRequest(
            "Credential Grant identity, capability, and destination are required".into(),
        ));
    }
    Ok(())
}

fn ensure_current_credential_grant_context(
    connection: &Connection,
    commission_id: &str,
    grant: &CredentialGrantRequest,
    expected_revision: i64,
) -> Result<(), TyrionError> {
    let context = connection
        .query_row(
            "SELECT commissions.status, commissions.revision, assignments.plan_revision,
                    attempts.status, worker_leases.status, worker_leases.expires_at,
                    worker_leases.mandate_revision
             FROM commissions
             JOIN assignments ON assignments.commission_id = commissions.id
             JOIN attempts ON attempts.assignment_id = assignments.id
             JOIN worker_leases ON worker_leases.attempt_id = attempts.id
             WHERE commissions.id = ?1 AND assignments.id = ?2
               AND attempts.id = ?3 AND worker_leases.id = ?4",
            params![
                commission_id,
                grant.assignment_id,
                grant.attempt_id,
                grant.worker_lease_id,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::ControlDenied(
                "Credential Grant does not identify a current Assignment, Attempt, and Worker Lease"
                    .into(),
            )
        })?;
    if context.1 != expected_revision {
        return Err(TyrionError::StaleRevision {
            expected: expected_revision,
            actual: context.1,
        });
    }
    if context.0 != CommissionStatus::Active.as_str()
        || grant.mandate_revision != context.1
        || grant.plan_revision != context.2
        || context.3 != AttemptStatus::Running.as_str()
        || context.4 != WorkerLeaseStatus::Active.as_str()
        || context.6 != context.1
        || context.5 <= unix_timestamp()?
    {
        return Err(TyrionError::ControlDenied(
            "Credential Grant authority is stale or no longer active".into(),
        ));
    }
    Ok(())
}

fn load_current_credential_grant(
    connection: &Connection,
    commission_id: &str,
    operation: &OperationRequest,
) -> Result<StoredCredentialGrant, TyrionError> {
    let credential_use = operation.credential.as_ref().ok_or_else(|| {
        TyrionError::ControlDenied("credentialed operation has no Credential Grant".into())
    })?;
    let stored = connection
        .query_row(
            "SELECT id, assignment_id, attempt_id, worker_lease_id, mandate_revision,
                    plan_revision, credential_reference, capability, destination,
                    exposure, credential_expires_at, status
             FROM credential_grants
             WHERE id = ?1 AND commission_id = ?2",
            params![credential_use.grant_id, commission_id],
            |row| {
                Ok((
                    StoredCredentialGrant {
                        id: row.get(0)?,
                        credential_reference: row.get(6)?,
                        capability: row.get(7)?,
                        destination: row.get(8)?,
                        exposure: row.get(9)?,
                        expires_at: row.get(10)?,
                        status: row.get(11)?,
                    },
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            TyrionError::ControlDenied(
                "the Credential Grant is not bound to the exact current Assignment grant".into(),
            )
        })?;
    if stored.1 != operation.assignment_id
        || stored.2 != operation.attempt_id
        || stored.3 != operation.worker_lease_id
        || stored.4 != operation.mandate_revision
        || stored.5 != operation.plan_revision
    {
        return Err(TyrionError::ControlDenied(
            "the Credential Grant is not bound to the exact current Assignment grant".into(),
        ));
    }
    Ok(stored.0)
}

fn validate_credential_operation_grant(
    operation: &OperationRequest,
    grant: &StoredCredentialGrant,
) -> Result<(), TyrionError> {
    let credential_use = operation.credential.as_ref().ok_or_else(|| {
        TyrionError::ControlDenied("credentialed operation has no Credential Grant".into())
    })?;
    let mode_allowed = match credential_use.mode {
        CredentialUseMode::Brokered => grant.exposure == CredentialExposure::BrokeredOnly.as_str(),
        CredentialUseMode::OneShotExposure => {
            grant.exposure == CredentialExposure::OneShot.as_str()
        }
    };
    if grant.id != credential_use.grant_id
        || grant.status != "active"
        || grant.expires_at <= unix_timestamp()?
        || grant.capability != "http_bearer"
        || operation.destination.as_deref() != Some(grant.destination.as_str())
        || !mode_allowed
    {
        return Err(TyrionError::ControlDenied(
            "the Credential Grant is stale, consumed, mismatched, or does not permit this delivery mode"
                .into(),
        ));
    }
    Ok(())
}

fn execute_file_replacement(
    operation: &OperationRequest,
    commission_storage_ceiling: u64,
    binding: &FileEffectBinding,
    effect_deadline: i64,
    started: Instant,
    leave_started_after_rename: bool,
    hold_before_commit_milliseconds: u64,
) -> Result<Value, EffectExecutionError> {
    if operation.operation != "filesystem.write"
        || operation.effect.as_deref() != Some("filesystem.write")
        || operation.destination.as_deref() != Some("local")
        || operation.parameters.len() != 1
    {
        return Err(TyrionError::ControlDenied(
            "only the exact canonical filesystem.write effect can execute".into(),
        )
        .into());
    }
    let content = operation.parameters.get("content").ok_or_else(|| {
        TyrionError::InvalidRequest("filesystem.write requires one content parameter".into())
    })?;
    let content_bytes = content.as_bytes();
    if content_bytes.len() as u64 > operation.limits.max_output_bytes
        || content_bytes.len() as u64 > commission_storage_ceiling
    {
        return Err(TyrionError::InvalidRequest(
            "filesystem.write content exceeds the approved or Commission storage ceiling".into(),
        )
        .into());
    }
    let repository_metadata = fs::metadata(&binding.canonical_repository)?;
    if repository_metadata.dev() != binding.repository_device
        || repository_metadata.ino() != binding.repository_inode
    {
        return Err(TyrionError::ControlDenied(
            "the approved repository identity changed before execution".into(),
        )
        .into());
    }
    let directory = fs::File::from(openat(
        CWD,
        binding.canonical_parent.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let directory_metadata = directory.metadata()?;
    if directory_metadata.dev() != binding.parent_device
        || directory_metadata.ino() != binding.parent_inode
    {
        return Err(TyrionError::ControlDenied(
            "the approved target directory identity changed before execution".into(),
        )
        .into());
    }
    let target_name = Path::new(&operation.target)
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("effect target has no file name".into()))?;
    let metadata = ensure_bound_target(&directory, target_name, binding)?;
    let after_digest = format!("{:x}", Sha256::digest(content_bytes));
    let temporary = format!(".tyrion-effect-{}", Uuid::new_v4());
    let write_result = (|| -> Result<(), TyrionError> {
        let mut file = fs::File::from(
            openat(
                &directory,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?,
        );
        file.set_permissions(metadata.permissions())?;
        for chunk in content_bytes.chunks(64 * 1024) {
            ensure_effect_deadline(
                started,
                operation.limits.max_duration_seconds,
                effect_deadline,
            )?;
            file.write_all(chunk)?;
        }
        ensure_effect_deadline(
            started,
            operation.limits.max_duration_seconds,
            effect_deadline,
        )?;
        file.sync_all()?;
        ensure_effect_deadline(
            started,
            operation.limits.max_duration_seconds,
            effect_deadline,
        )?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), rustix::fs::AtFlags::empty());
        return Err(EffectExecutionError::Failed(error));
    }
    ensure_effect_deadline(
        started,
        operation.limits.max_duration_seconds,
        effect_deadline,
    )?;
    ensure_bound_target(&directory, target_name, binding)?;
    if hold_before_commit_milliseconds > 0 {
        std::thread::sleep(Duration::from_millis(hold_before_commit_milliseconds));
    }
    ensure_effect_deadline(
        started,
        operation.limits.max_duration_seconds,
        effect_deadline,
    )?;
    let displaced = format!(".tyrion-displaced-{}", Uuid::new_v4());
    if let Err(error) =
        rename_noreplace(&directory, target_name, &directory, OsStr::new(&displaced))
    {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), rustix::fs::AtFlags::empty());
        return Err(EffectExecutionError::Failed(error.into()));
    }
    if let Err(error) = ensure_bound_target(&directory, OsStr::new(&displaced), binding) {
        let restored =
            rename_noreplace(&directory, OsStr::new(&displaced), &directory, target_name).is_ok();
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), rustix::fs::AtFlags::empty());
        if restored {
            return Err(EffectExecutionError::Failed(error));
        }
        return Err(uncertain_file_effect(
            error,
            UncertainEffectDetails {
                operation,
                binding,
                bytes_written: content_bytes.len(),
                after_sha256: &after_digest,
                started,
                effect_observed_after_rename: false,
                requirement: "The target raced after approval and the approved inode could not be restored; reconcile it before resuming.",
            },
        ));
    }
    if let Err(error) = ensure_effect_deadline(
        started,
        operation.limits.max_duration_seconds,
        effect_deadline,
    ) {
        let restored =
            rename_noreplace(&directory, OsStr::new(&displaced), &directory, target_name).is_ok();
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), rustix::fs::AtFlags::empty());
        if restored {
            return Err(EffectExecutionError::Failed(error));
        }
        return Err(uncertain_file_effect(
            error,
            UncertainEffectDetails {
                operation,
                binding,
                bytes_written: content_bytes.len(),
                after_sha256: &after_digest,
                started,
                effect_observed_after_rename: false,
                requirement: "The approved inode could not be restored after the deadline; reconcile it before resuming.",
            },
        ));
    }
    if let Err(error) =
        rename_noreplace(&directory, OsStr::new(&temporary), &directory, target_name)
    {
        let restored =
            rename_noreplace(&directory, OsStr::new(&displaced), &directory, target_name).is_ok();
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), rustix::fs::AtFlags::empty());
        if restored {
            return Err(EffectExecutionError::Failed(error.into()));
        }
        return Err(uncertain_file_effect(
            error.into(),
            UncertainEffectDetails {
                operation,
                binding,
                bytes_written: content_bytes.len(),
                after_sha256: &after_digest,
                started,
                effect_observed_after_rename: false,
                requirement: "A concurrent target appeared and the approved inode could not be restored; reconcile it before resuming.",
            },
        ));
    }
    if let Err(error) =
        rustix::fs::unlinkat(&directory, displaced.as_str(), rustix::fs::AtFlags::empty())
    {
        return Err(uncertain_file_effect(
            std::io::Error::from(error).into(),
            UncertainEffectDetails {
                operation,
                binding,
                bytes_written: content_bytes.len(),
                after_sha256: &after_digest,
                started,
                effect_observed_after_rename: true,
                requirement: "The approved prior inode could not be removed after replacement; reconcile the target before resuming.",
            },
        ));
    }
    if let Err(error) = directory.sync_all() {
        return Err(uncertain_file_effect(
            error.into(),
            UncertainEffectDetails {
                operation,
                binding,
                bytes_written: content_bytes.len(),
                after_sha256: &after_digest,
                started,
                effect_observed_after_rename: true,
                requirement: "Reconcile the exact target before any linked Commission retries it.",
            },
        ));
    }
    if leave_started_after_rename {
        return Err(EffectExecutionError::LeaveStartedAfterEffect);
    }
    if started.elapsed() > Duration::from_secs(operation.limits.max_duration_seconds) {
        return Err(EffectExecutionError::Uncertain {
            error: TyrionError::InvalidRequest(
                "filesystem.write exceeded its exact duration limit while containing the committed effect"
                    .into(),
            ),
            receipt: serde_json::json!({
                "status": "uncertain",
                "operation": operation.operation,
                "repository": binding.canonical_repository,
                "target": operation.target,
                "bytes_written": content_bytes.len(),
                "before_sha256": binding.before_sha256,
                "after_sha256": after_digest,
                "duration_millis": started.elapsed().as_millis(),
                "effect_observed_after_rename": true,
                "requirement": "Reconcile the exact target before any linked Commission retries it.",
                "credential_used": false,
                "secret_material_retained": false,
            }),
        });
    }
    Ok(serde_json::json!({
        "status": "confirmed",
        "operation": operation.operation,
        "repository": binding.canonical_repository,
        "target": operation.target,
        "bytes_written": content_bytes.len(),
        "before_sha256": binding.before_sha256,
        "after_sha256": after_digest,
        "duration_millis": started.elapsed().as_millis(),
        "credential_used": false,
        "secret_material_retained": false,
    }))
}

fn ensure_effect_deadline(
    started: Instant,
    max_duration_seconds: u64,
    effect_deadline: i64,
) -> Result<(), TyrionError> {
    if started.elapsed() >= Duration::from_secs(max_duration_seconds) {
        return Err(TyrionError::InvalidRequest(
            "filesystem.write reached its exact duration limit before committing the effect".into(),
        ));
    }
    if unix_timestamp()? >= effect_deadline {
        return Err(TyrionError::ControlDenied(
            "filesystem.write reached its Worker Lease or Commission deadline before committing the effect"
                .into(),
        ));
    }
    Ok(())
}

fn rename_noreplace(
    old_directory: &fs::File,
    old_name: &OsStr,
    new_directory: &fs::File,
    new_name: &OsStr,
) -> std::io::Result<()> {
    let old_name = CString::new(old_name.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    let new_name = CString::new(new_name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains NUL",
        )
    })?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            old_directory.as_raw_fd(),
            old_name.as_ptr(),
            new_directory.as_raw_fd(),
            new_name.as_ptr(),
            0x0000_0004,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_directory.as_raw_fd(),
            old_name.as_ptr(),
            new_directory.as_raw_fd(),
            new_name.as_ptr(),
            1_u32,
        ) as libc::c_int
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ));
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

struct UncertainEffectDetails<'a> {
    operation: &'a OperationRequest,
    binding: &'a FileEffectBinding,
    bytes_written: usize,
    after_sha256: &'a str,
    started: Instant,
    effect_observed_after_rename: bool,
    requirement: &'a str,
}

fn uncertain_file_effect(
    error: TyrionError,
    details: UncertainEffectDetails<'_>,
) -> EffectExecutionError {
    EffectExecutionError::Uncertain {
        error,
        receipt: serde_json::json!({
            "status": "uncertain",
            "operation": details.operation.operation,
            "repository": details.binding.canonical_repository,
            "target": details.operation.target,
            "bytes_written": details.bytes_written,
            "before_sha256": details.binding.before_sha256,
            "after_sha256": details.after_sha256,
            "duration_millis": details.started.elapsed().as_millis(),
            "effect_observed_after_rename": details.effect_observed_after_rename,
            "requirement": details.requirement,
            "credential_used": false,
            "secret_material_retained": false,
        }),
    }
}

enum EffectExecutionError {
    Failed(TyrionError),
    Uncertain { error: TyrionError, receipt: Value },
    LeaveStartedAfterEffect,
}

fn credential_execution_contained(execution: &Result<Value, EffectExecutionError>) -> bool {
    let receipt = match execution {
        Ok(receipt) | Err(EffectExecutionError::Uncertain { receipt, .. }) => receipt,
        Err(EffectExecutionError::Failed(_) | EffectExecutionError::LeaveStartedAfterEffect) => {
            return false
        }
    };
    receipt
        .get("broker_process_contained")
        .or_else(|| {
            receipt
                .get("external_response")
                .and_then(|external| external.get("broker_process_contained"))
        })
        .and_then(Value::as_bool)
        == Some(true)
}

fn active_broker_process<'a>(
    process_id: Option<u32>,
    marker: Option<&'a str>,
    status: Option<&str>,
) -> Result<Option<(u32, &'a str)>, TyrionError> {
    match (status, process_id, marker) {
        (Some("active"), Some(process_id), Some(marker)) => Ok(Some((process_id, marker))),
        (Some("contained"), _, _) | (None, None, None) => Ok(None),
        _ => Err(TyrionError::ControlDenied(
            "credential broker process identity is incomplete or inconsistent".into(),
        )),
    }
}

fn ensure_bound_target(
    directory: &fs::File,
    target_name: &std::ffi::OsStr,
    binding: &FileEffectBinding,
) -> Result<fs::Metadata, TyrionError> {
    let mut target = fs::File::from(
        openat(
            directory,
            target_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let metadata = target.metadata()?;
    if !metadata.is_file()
        || metadata.dev() != binding.target_device
        || metadata.ino() != binding.target_inode
        || sha256_reader(&mut target)? != binding.before_sha256
    {
        return Err(TyrionError::ControlDenied(
            "the exact approved target revision changed before execution".into(),
        ));
    }
    Ok(metadata)
}

impl From<TyrionError> for EffectExecutionError {
    fn from(error: TyrionError) -> Self {
        Self::Failed(error)
    }
}

impl From<std::io::Error> for EffectExecutionError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.into())
    }
}

impl From<rustix::io::Errno> for EffectExecutionError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Failed(std::io::Error::from(error).into())
    }
}

impl From<CredentialEffectError> for EffectExecutionError {
    fn from(error: CredentialEffectError) -> Self {
        match error {
            CredentialEffectError::Failed(error) => Self::Failed(error),
            CredentialEffectError::Uncertain { error, receipt } => {
                Self::Uncertain { error, receipt }
            }
            CredentialEffectError::LeaveStartedAfterEffect => Self::LeaveStartedAfterEffect,
        }
    }
}

fn ensure_acyclic_plan(plan: &CommissionPlan) -> Result<(), TyrionError> {
    fn visit<'a>(
        id: &'a str,
        assignments: &HashMap<&'a str, &'a PlannedAssignment>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
    ) -> bool {
        if visited.contains(id) {
            return true;
        }
        if !visiting.insert(id) {
            return false;
        }
        let acyclic = assignments[id]
            .dependencies
            .iter()
            .all(|dependency| visit(dependency, assignments, visiting, visited));
        visiting.remove(id);
        if acyclic {
            visited.insert(id);
        }
        acyclic
    }

    let assignments = plan
        .assignments
        .iter()
        .map(|assignment| (assignment.id.as_str(), assignment))
        .collect::<HashMap<_, _>>();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    if assignments
        .keys()
        .all(|id| visit(id, &assignments, &mut visiting, &mut visited))
    {
        Ok(())
    } else {
        Err(TyrionError::InvalidRequest(
            "Commission Plan dependencies must be acyclic".into(),
        ))
    }
}

fn validate_acceptance_criteria(
    execution: &ExecutionSpec,
    criteria: &[AcceptanceCriterion],
) -> Result<(), TyrionError> {
    if criteria.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "at least one acceptance criterion is required".into(),
        ));
    }
    let mut criterion_ids = HashSet::new();
    for criterion in criteria {
        if criterion.id.trim().is_empty()
            || criterion.description.trim().is_empty()
            || criterion.required_evidence.trim().is_empty()
            || criterion.verification_environment.trim().is_empty()
        {
            return Err(TyrionError::InvalidRequest(
                "criterion id, description, required Evidence, and verification environment must not be empty".into(),
            ));
        }
        if !criterion_ids.insert(&criterion.id) {
            return Err(TyrionError::InvalidRequest(format!(
                "criterion id {} is duplicated",
                criterion.id
            )));
        }
        match (criterion.verifier_type, &criterion.verifier) {
            (
                VerifierType::Deterministic,
                Verifier::ExactMatch { .. } | Verifier::Command { .. },
            )
            | (VerifierType::Model | VerifierType::Principal, Verifier::Prompt { .. }) => {}
            (VerifierType::Deterministic, Verifier::Prompt { .. }) => {
                return Err(TyrionError::InvalidRequest(
                    "deterministic verifiers require an exact_match or command procedure".into(),
                ));
            }
            (VerifierType::Model | VerifierType::Principal, _) => {
                return Err(TyrionError::InvalidRequest(
                    "model and Principal verifiers require a prompt procedure".into(),
                ));
            }
        }
        match (execution, criterion.verifier_type, &criterion.verifier) {
            (
                ExecutionSpec::Deterministic,
                VerifierType::Deterministic,
                Verifier::ExactMatch { .. },
            )
            | (
                ExecutionSpec::CodexGit { .. },
                VerifierType::Deterministic,
                Verifier::Command { .. },
            )
            | (_, VerifierType::Model | VerifierType::Principal, Verifier::Prompt { .. }) => {}
            (
                ExecutionSpec::Deterministic,
                VerifierType::Deterministic,
                Verifier::Command { .. },
            ) => {
                return Err(TyrionError::InvalidRequest(
                    "deterministic execution requires exact_match verifiers".into(),
                ));
            }
            (
                ExecutionSpec::CodexGit { .. },
                VerifierType::Deterministic,
                Verifier::ExactMatch { .. },
            ) => {
                return Err(TyrionError::InvalidRequest(
                    "codex_git execution requires command verifiers".into(),
                ));
            }
            _ => unreachable!("verifier type and procedure were validated"),
        }
        if let Verifier::Command { argv } = &criterion.verifier {
            if argv.is_empty()
                || argv
                    .iter()
                    .any(|argument| argument.is_empty() || argument.contains('\0'))
            {
                return Err(TyrionError::InvalidRequest(
                    "command verifier argv must contain only non-empty arguments".into(),
                ));
            }
        }
        if let Verifier::Prompt { prompt } = &criterion.verifier {
            if prompt.trim().is_empty() {
                return Err(TyrionError::InvalidRequest(
                    "prompt verifier procedure must not be empty".into(),
                ));
            }
        }
    }
    Ok(())
}

fn resolved_verifier_configuration(criterion: &crate::protocol::AcceptanceCriterion) -> String {
    if !criterion.verifier_configuration.trim().is_empty() {
        return criterion.verifier_configuration.clone();
    }
    match criterion.verifier {
        Verifier::ExactMatch { .. } => "deterministic-exact-match-v1".into(),
        Verifier::Command { .. } => "contained-command-v1".into(),
        Verifier::Prompt { .. } => match criterion.verifier_type {
            VerifierType::Model => "model-verifier-v1".into(),
            VerifierType::Principal => "principal-verifier-v1".into(),
            VerifierType::Deterministic => unreachable!("validated verifier type"),
        },
    }
}

fn insert_criterion_version(
    transaction: &Transaction<'_>,
    commission_id: &str,
    mandate_revision: i64,
    position: usize,
    criterion: &AcceptanceCriterion,
) -> Result<(), TyrionError> {
    let (verifier_kind, expected) = verifier_storage(&criterion.verifier)?;
    transaction.execute(
        "INSERT INTO criterion_versions (
            commission_id, mandate_revision, criterion_id, position, description,
            required_evidence, verifier_type, verification_depth,
            verifier_configuration, verification_environment, verifier_kind, expected
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            commission_id,
            mandate_revision,
            criterion.id,
            position as i64,
            criterion.description,
            criterion.required_evidence,
            criterion.verifier_type.as_str(),
            criterion.verification_depth.as_str(),
            resolved_verifier_configuration(criterion),
            criterion.verification_environment,
            verifier_kind,
            expected,
        ],
    )?;
    Ok(())
}

fn validate_attachment_identity(identity: &AdapterIdentity) -> Result<(), TyrionError> {
    if identity.harness.trim().is_empty()
        || identity.adapter_identity.trim().is_empty()
        || identity.adapter_version.trim().is_empty()
    {
        return Err(TyrionError::InvalidRequest(
            "harness, adapter identity, and adapter version must not be empty".into(),
        ));
    }
    Ok(())
}

fn attachment_token_hash(launch_token: &str) -> String {
    format!("{:x}", Sha256::digest(launch_token.as_bytes()))
}

fn ensure_result_fits_storage_ceiling(
    goal: &str,
    max_storage_bytes: u64,
) -> Result<(), TyrionError> {
    if goal.len() as u64 > max_storage_bytes {
        return Err(TyrionError::InvalidRequest(format!(
            "max_storage_bytes is {max_storage_bytes}, but the deterministic Result requires {} bytes",
            goal.len()
        )));
    }
    Ok(())
}

fn mutation_key(request: &Request) -> Result<&str, TyrionError> {
    request
        .idempotency_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            TyrionError::InvalidRequest("mutating requests require an idempotency key".into())
        })
}

fn request_hash(request: &Request) -> Result<String, TyrionError> {
    let encoded = serde_json::to_vec(request)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn prior_result(
    transaction: &Transaction<'_>,
    key: &str,
    expected_hash: &str,
) -> Result<Option<Value>, TyrionError> {
    let existing = transaction
        .query_row(
            "SELECT request_hash, response_json FROM idempotency_keys WHERE key = ?1",
            [key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match existing {
        Some((actual_hash, response)) if actual_hash == expected_hash => {
            Ok(Some(serde_json::from_str(&response)?))
        }
        Some(_) => Err(TyrionError::IdempotencyConflict),
        None => Ok(None),
    }
}

fn save_idempotent_result(
    transaction: &Transaction<'_>,
    key: &str,
    request_hash: &str,
    result: &Value,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO idempotency_keys (key, request_hash, response_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![key, request_hash, serde_json::to_string(result)?, unix_timestamp()?],
    )?;
    Ok(())
}

fn insert_authority(
    transaction: &Transaction<'_>,
    commission_id: &str,
    proposal: &CommissionProposal,
) -> Result<(), TyrionError> {
    let scopes = [
        (
            AuthorityScopeType::Repository,
            &proposal.authority.repositories,
        ),
        (AuthorityScopeType::Path, &proposal.authority.paths),
        (AuthorityScopeType::Action, &proposal.authority.actions),
        (
            AuthorityScopeType::Destination,
            &proposal.authority.destinations,
        ),
        (AuthorityScopeType::Effect, &proposal.authority.effects),
    ];
    for (scope_type, values) in scopes {
        for (position, value) in values.iter().enumerate() {
            transaction.execute(
                "INSERT INTO authority_scopes (commission_id, scope_type, position, value) VALUES (?1, ?2, ?3, ?4)",
                params![commission_id, scope_type.as_str(), position as i64, value],
            )?;
        }
    }
    Ok(())
}

fn insert_resource_ceilings(
    transaction: &Transaction<'_>,
    commission_id: &str,
    ceilings: &ResourceCeilings,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO resource_ceilings (
            commission_id, max_attempts, max_elapsed_seconds, max_worker_concurrency,
            max_storage_bytes, max_model_spend_cents, max_paid_service_spend_cents
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            commission_id,
            ceilings.max_attempts,
            ceilings.max_elapsed_seconds,
            ceilings.max_worker_concurrency,
            ceilings.max_storage_bytes,
            ceilings.max_model_spend_cents,
            ceilings.max_paid_service_spend_cents,
        ],
    )?;
    Ok(())
}

fn record_event(
    transaction: &Transaction<'_>,
    commission_id: &str,
    event_kind: EventKind,
    revision: i64,
) -> Result<(), TyrionError> {
    record_event_with_payload(
        transaction,
        commission_id,
        event_kind,
        revision,
        &serde_json::json!({}),
    )
}

fn record_event_with_payload(
    transaction: &Transaction<'_>,
    commission_id: &str,
    event_kind: EventKind,
    revision: i64,
    payload: &Value,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO events (
            commission_id, event_type, commission_revision, payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            commission_id,
            event_kind.as_str(),
            revision,
            serde_json::to_string(payload)?,
            unix_timestamp()?
        ],
    )?;
    Ok(())
}

fn unix_timestamp() -> Result<i64, TyrionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            TyrionError::InvalidRequest(format!("system clock is invalid: {error}"))
        })?;
    Ok(duration.as_secs() as i64)
}

fn next_worker_handle(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<String, TyrionError> {
    const HANDLES: [&str; 16] = [
        "Arya",
        "Brienne",
        "Davos",
        "Gendry",
        "Grey Worm",
        "Jaime",
        "Jon",
        "Meera",
        "Missandei",
        "Podrick",
        "Samwell",
        "Sansa",
        "Theon",
        "Tormund",
        "Tyrion",
        "Yara",
    ];
    let count = transaction.query_row(
        "SELECT COUNT(*) FROM workers WHERE commission_id = ?1",
        [commission_id],
        |row| row.get::<_, usize>(0),
    )?;
    let base = HANDLES[count % HANDLES.len()];
    Ok(if count < HANDLES.len() {
        base.to_owned()
    } else {
        format!("{base} {}", count / HANDLES.len() + 1)
    })
}

fn unix_timestamp_millis() -> Result<i64, TyrionError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TyrionError::InvalidRequest("system clock is before Unix epoch".into()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| TyrionError::InvalidRequest("system clock does not fit in SQLite".into()))
}

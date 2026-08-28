use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{backup::Backup, Connection, OptionalExtension};

use crate::TyrionError;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commissions (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'proposed', 'active', 'paused', 'cancelled', 'verified_complete'
    )),
    revision INTEGER NOT NULL,
    control_revision INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    accepted_at INTEGER,
    completed_at INTEGER,
    artifact_revision TEXT,
    execution_json TEXT NOT NULL DEFAULT '{"kind":"deterministic"}',
    worker_requirements_json TEXT NOT NULL DEFAULT '{}',
    plan_json TEXT,
    project_id TEXT,
    commission_constraints_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS projects (
    project_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS project_identities (
    project_id TEXT NOT NULL REFERENCES projects(project_id),
    repository_device INTEGER NOT NULL,
    repository_inode INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, repository_device, repository_inode),
    UNIQUE (repository_device, repository_inode)
);

CREATE TABLE IF NOT EXISTS project_aliases (
    project_id TEXT NOT NULL,
    repository_device INTEGER NOT NULL,
    repository_inode INTEGER NOT NULL,
    canonical_repository TEXT NOT NULL UNIQUE,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, canonical_repository),
    FOREIGN KEY (project_id, repository_device, repository_inode)
        REFERENCES project_identities(project_id, repository_device, repository_inode)
);

CREATE TABLE IF NOT EXISTS profile_claims (
    id TEXT PRIMARY KEY,
    current_version INTEGER NOT NULL CHECK (current_version > 0),
    strength TEXT NOT NULL CHECK (strength IN ('hard', 'soft')),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('principal', 'project')),
    scope_id TEXT REFERENCES projects(project_id),
    applicability TEXT NOT NULL CHECK (applicability = 'software_building'),
    confidence_category TEXT NOT NULL CHECK (confidence_category IN ('explicit', 'inferred')),
    confidence_basis_points INTEGER NOT NULL CHECK (
        confidence_basis_points >= 0 AND confidence_basis_points <= 10000
    ),
    lifecycle_state TEXT NOT NULL CHECK (lifecycle_state IN (
        'candidate', 'active', 'suppressed', 'contradicted'
    )),
    statement_fingerprint TEXT NOT NULL,
    last_nonweak_support_at INTEGER,
    lifecycle_changed_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (
        (scope_kind = 'principal' AND scope_id IS NULL)
        OR (scope_kind = 'project' AND scope_id IS NOT NULL)
    ),
    FOREIGN KEY (id, current_version)
        REFERENCES profile_claim_versions(claim_id, version)
);

CREATE TABLE IF NOT EXISTS profile_claim_versions (
    claim_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    statement TEXT NOT NULL,
    token_upper_bound INTEGER NOT NULL CHECK (token_upper_bound > 0),
    provenance_commission_id TEXT NOT NULL REFERENCES commissions(id),
    provenance_attachment_id TEXT NOT NULL REFERENCES attachments(id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (claim_id, version)
);

CREATE TRIGGER IF NOT EXISTS profile_claim_versions_are_immutable_update
BEFORE UPDATE ON profile_claim_versions
BEGIN
    SELECT RAISE(ABORT, 'Profile Claim versions are immutable');
END;

CREATE TABLE IF NOT EXISTS criteria (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    required_evidence TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
    verifier_configuration TEXT NOT NULL,
    verification_environment TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    expected TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('uncertain', 'passed', 'failed')),
    PRIMARY KEY (commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS criterion_versions (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    mandate_revision INTEGER NOT NULL,
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    required_evidence TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
    verifier_configuration TEXT NOT NULL,
    verification_environment TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    expected TEXT NOT NULL,
    PRIMARY KEY (commission_id, mandate_revision, criterion_id)
);

CREATE TABLE IF NOT EXISTS authority_scopes (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    scope_type TEXT NOT NULL CHECK (scope_type IN ('repository', 'path', 'action', 'destination', 'effect')),
    position INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (commission_id, scope_type, position)
);

CREATE TABLE IF NOT EXISTS resource_ceilings (
    commission_id TEXT PRIMARY KEY REFERENCES commissions(id),
    max_attempts INTEGER NOT NULL,
    max_elapsed_seconds INTEGER NOT NULL,
    max_worker_concurrency INTEGER NOT NULL,
    max_storage_bytes INTEGER NOT NULL,
    max_model_spend_cents INTEGER NOT NULL,
    max_paid_service_spend_cents INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS known_uncertainties (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    PRIMARY KEY (commission_id, position)
);

CREATE TABLE IF NOT EXISTS commission_plans (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    revision INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('entry_model', 'control_plane')),
    reason TEXT NOT NULL,
    snapshot_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (commission_id, revision)
);

CREATE TABLE IF NOT EXISTS planned_assignments (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    logical_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    goal TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN (
        'critical_path', 'uncertainty_reduction', 'independent_verification', 'reconciliation'
    )),
    read_scopes_json TEXT NOT NULL,
    write_scopes_json TEXT NOT NULL,
    concurrency_slots INTEGER NOT NULL,
    max_storage_bytes INTEGER NOT NULL,
    max_model_spend_cents INTEGER NOT NULL,
    max_paid_service_spend_cents INTEGER NOT NULL,
    worker_requirements_json TEXT NOT NULL DEFAULT '{}',
    competition_group TEXT,
    competition_uncertainty TEXT,
    competition_rule TEXT,
    created_plan_revision INTEGER NOT NULL,
    PRIMARY KEY (commission_id, logical_id)
);

CREATE TABLE IF NOT EXISTS planned_assignment_dependencies (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_logical_id TEXT NOT NULL,
    dependency_logical_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (commission_id, assignment_logical_id, dependency_logical_id),
    FOREIGN KEY (commission_id, assignment_logical_id)
        REFERENCES planned_assignments(commission_id, logical_id),
    FOREIGN KEY (commission_id, dependency_logical_id)
        REFERENCES planned_assignments(commission_id, logical_id)
);

CREATE TABLE IF NOT EXISTS planned_assignment_criteria (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_logical_id TEXT NOT NULL,
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (commission_id, assignment_logical_id, criterion_id),
    FOREIGN KEY (commission_id, assignment_logical_id)
        REFERENCES planned_assignments(commission_id, logical_id),
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS assignments (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    plan_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'ready', 'running', 'accepted', 'superseded', 'verification_pending', 'verification_failed',
        'resource_blocked', 'attention_required', 'cancelled'
    )),
    created_at INTEGER NOT NULL,
    UNIQUE (id, commission_id)
);

CREATE TABLE IF NOT EXISTS assignment_metadata (
    assignment_id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL,
    logical_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    goal TEXT NOT NULL,
    purpose TEXT NOT NULL,
    read_scopes_json TEXT NOT NULL,
    write_scopes_json TEXT NOT NULL,
    concurrency_slots INTEGER NOT NULL,
    max_storage_bytes INTEGER NOT NULL,
    max_model_spend_cents INTEGER NOT NULL,
    max_paid_service_spend_cents INTEGER NOT NULL,
    competition_group TEXT,
    competition_uncertainty TEXT,
    competition_rule TEXT,
    legacy INTEGER NOT NULL CHECK (legacy IN (0, 1)),
    UNIQUE (assignment_id, logical_id),
    FOREIGN KEY (assignment_id, commission_id) REFERENCES assignments(id, commission_id),
    FOREIGN KEY (commission_id, logical_id)
        REFERENCES planned_assignments(commission_id, logical_id)
);

CREATE TABLE IF NOT EXISTS attempts (
    id TEXT PRIMARY KEY,
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    worker_configuration TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'running', 'succeeded', 'failed', 'interrupted', 'timed_out', 'cancelled'
    )),
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    started_at_ms INTEGER NOT NULL,
    execution_completed_at_ms INTEGER,
    completed_at_ms INTEGER,
    revision_disposition TEXT NOT NULL DEFAULT 'current' CHECK (revision_disposition IN (
        'current', 'retained', 'superseded', 'stale', 'requires_revalidation'
    ))
);

CREATE TABLE IF NOT EXISTS attempt_context_packets (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    packet_json TEXT NOT NULL,
    advisory_token_budget INTEGER NOT NULL,
    advisory_tokens_used INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attempt_profile_claims (
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    claim_id TEXT NOT NULL,
    claim_version INTEGER NOT NULL,
    position INTEGER NOT NULL,
    result_id TEXT REFERENCES results(id),
    outcome TEXT CHECK (outcome IN ('accepted', 'edited', 'rejected', 'contradicted')),
    recorded_at INTEGER,
    PRIMARY KEY (attempt_id, claim_id),
    FOREIGN KEY (claim_id, claim_version)
        REFERENCES profile_claim_versions(claim_id, version)
);

CREATE TABLE IF NOT EXISTS assignment_routes (
    assignment_id TEXT PRIMARY KEY REFERENCES assignments(id),
    status TEXT NOT NULL CHECK (status IN ('selected', 'attention_required')),
    selected_configuration_json TEXT,
    rationale_json TEXT NOT NULL,
    decided_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS assignment_skill_defaults (
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    skill_name TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    requirement TEXT NOT NULL CHECK (requirement IN ('required', 'selected')),
    provenance TEXT NOT NULL CHECK (provenance IN ('principal', 'plan', 'worker')),
    plan_revision INTEGER NOT NULL,
    delegation TEXT NOT NULL CHECK (delegation = 'native_unchanged'),
    selected_at INTEGER NOT NULL,
    PRIMARY KEY (assignment_id, skill_name)
);

CREATE TRIGGER IF NOT EXISTS assignment_skill_defaults_are_immutable_update
BEFORE UPDATE ON assignment_skill_defaults
BEGIN
    SELECT RAISE(ABORT, 'Assignment Skill defaults are immutable');
END;

CREATE TRIGGER IF NOT EXISTS assignment_skill_defaults_are_immutable_delete
BEFORE DELETE ON assignment_skill_defaults
BEGIN
    SELECT RAISE(ABORT, 'Assignment Skill defaults are immutable');
END;

CREATE TABLE IF NOT EXISTS attention_conditions (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    code TEXT NOT NULL,
    requirement TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open', 'resolved')),
    created_at INTEGER NOT NULL,
    resolved_at INTEGER
);

CREATE TABLE IF NOT EXISTS workers (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    handle TEXT NOT NULL,
    configuration_json TEXT NOT NULL,
    routing_rationale_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'running', 'succeeded', 'failed', 'interrupted', 'timed_out', 'cancelled'
    )),
    native_session_id TEXT,
    latest_activity TEXT NOT NULL,
    activity_at_ms INTEGER NOT NULL,
    usage_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE (commission_id, handle)
);

CREATE TABLE IF NOT EXISTS worker_commands (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    worker_id TEXT NOT NULL REFERENCES workers(id),
    attachment_id TEXT NOT NULL REFERENCES attachments(id),
    kind TEXT NOT NULL CHECK (kind IN ('steer', 'interrupt')),
    payload_json TEXT NOT NULL,
    mandate_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'delivered', 'failed')),
    idempotency_key TEXT NOT NULL UNIQUE,
    request_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS resource_reservations (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    concurrency_slots INTEGER NOT NULL,
    storage_bytes INTEGER NOT NULL,
    model_spend_cents INTEGER NOT NULL,
    paid_service_spend_cents INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'released', 'revoked')),
    reserved_at INTEGER NOT NULL,
    released_at INTEGER
);

CREATE TABLE IF NOT EXISTS worker_leases (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    mandate_revision INTEGER NOT NULL,
    released_at INTEGER,
    status TEXT NOT NULL CHECK (status IN ('active', 'released', 'revoked', 'expired'))
);

CREATE TABLE IF NOT EXISTS sandbox_cleanups (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS worker_configuration_failures (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    configuration_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (assignment_id, configuration_id)
);

CREATE TABLE IF NOT EXISTS attempt_recoveries (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
    cause TEXT NOT NULL,
    classification TEXT NOT NULL CHECK (classification IN (
        'transient', 'repairable_context', 'poor_fit', 'resource', 'authority',
        'interrupted', 'cancelled'
    )),
    equivalence_key TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN (
        'retry', 'reroute', 'replan', 'block', 'await_principal', 'cancel'
    )),
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS restart_recoveries (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    decision TEXT NOT NULL CHECK (
        decision IN ('reattach', 'expire_and_retry', 'expire_and_replan', 'expire_and_block')
    ),
    process_identity INTEGER NOT NULL CHECK (process_identity IN (0, 1)),
    native_session_identity INTEGER NOT NULL CHECK (native_session_identity IN (0, 1)),
    acknowledged_state INTEGER NOT NULL CHECK (acknowledged_state IN (0, 1)),
    lease_validity INTEGER NOT NULL CHECK (lease_validity IN (0, 1)),
    current_authority INTEGER NOT NULL CHECK (current_authority IN (0, 1)),
    containment INTEGER NOT NULL CHECK (containment IN (0, 1)),
    cleanup_confirmed INTEGER NOT NULL CHECK (cleanup_confirmed IN (0, 1)),
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS watchdog_findings (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    signal TEXT NOT NULL CHECK (signal IN (
        'stall', 'unhealthy_retry_pattern', 'repeated_verification_failure',
        'abnormal_resource_use', 'lost_liveness', 'invalid_authority'
    )),
    action TEXT NOT NULL CHECK (action = 'contain_attempt'),
    details TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (attempt_id, signal)
);

CREATE TABLE IF NOT EXISTS results (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    output TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'superseded')),
    created_at INTEGER NOT NULL,
    mandate_revision INTEGER,
    plan_revision INTEGER,
    base_revision TEXT,
    candidate_commits_json TEXT NOT NULL DEFAULT '[]',
    changed_paths_json TEXT NOT NULL DEFAULT '[]',
    artifacts_json TEXT NOT NULL DEFAULT '[]',
    verification_outcomes_json TEXT NOT NULL DEFAULT '[]',
    known_effects_json TEXT NOT NULL DEFAULT '[]',
    integrated_artifact_revision TEXT,
    revision_disposition TEXT NOT NULL DEFAULT 'current' CHECK (revision_disposition IN (
        'current', 'retained', 'superseded', 'stale', 'requires_revalidation'
    ))
);

CREATE TABLE IF NOT EXISTS result_skill_executions (
    result_id TEXT NOT NULL REFERENCES results(id),
    skill_name TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    requirement TEXT NOT NULL CHECK (requirement IN ('required', 'selected')),
    provenance TEXT NOT NULL CHECK (provenance IN ('principal', 'plan', 'worker')),
    worker_configuration TEXT NOT NULL,
    assignment_class TEXT NOT NULL,
    verification_outcome TEXT NOT NULL CHECK (verification_outcome IN ('passed', 'failed', 'uncertain')),
    corrections INTEGER NOT NULL,
    cost_cents INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    principal_intervention INTEGER NOT NULL CHECK (principal_intervention IN (0, 1)),
    delegation TEXT NOT NULL CHECK (delegation = 'native_unchanged'),
    PRIMARY KEY (result_id, skill_name)
);

CREATE TABLE IF NOT EXISTS skill_associations (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    result_id TEXT REFERENCES results(id),
    skill_name TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    worker_configuration TEXT NOT NULL,
    harness TEXT NOT NULL,
    assignment_class TEXT NOT NULL,
    observation TEXT NOT NULL CHECK (observation IN (
        'verified_success', 'verification_failure', 'required_skill_failure'
    )),
    verification_outcome TEXT NOT NULL CHECK (verification_outcome IN ('passed', 'failed', 'uncertain')),
    corrections INTEGER NOT NULL,
    cost_cents INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    principal_intervention INTEGER NOT NULL CHECK (principal_intervention IN (0, 1)),
    evidence_ids_json TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    confidence_basis_points INTEGER NOT NULL CHECK (
        confidence_basis_points BETWEEN 0 AND 10000
    ),
    observed_at INTEGER NOT NULL,
    UNIQUE (attempt_id, skill_name, observation)
);

CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    mandate_revision INTEGER NOT NULL,
    artifact_revision TEXT NOT NULL,
    evidence_type TEXT NOT NULL,
    verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
    scope TEXT NOT NULL CHECK (scope IN ('candidate', 'integrated', 'external')),
    verification_attempt_id TEXT NOT NULL,
    verifier_identity TEXT NOT NULL,
    verifier_configuration TEXT NOT NULL,
    verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
    procedure_json TEXT NOT NULL,
    environment TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed', 'uncertain')),
    observed TEXT NOT NULL,
    expected TEXT NOT NULL,
    material_contradiction INTEGER NOT NULL CHECK (material_contradiction IN (0, 1)),
    defect TEXT CHECK (defect IN ('result', 'verifier', 'environment', 'criterion')),
    producer_attempt_id TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (commission_id, criterion_id, evidence_type, verification_attempt_id),
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TRIGGER IF NOT EXISTS evidence_is_immutable_update
BEFORE UPDATE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'Evidence is immutable');
END;

CREATE TRIGGER IF NOT EXISTS evidence_is_immutable_delete
BEFORE DELETE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'Evidence is immutable');
END;

CREATE TABLE IF NOT EXISTS verification_gates (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    mandate_revision INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open', 'closed')),
    opened_at INTEGER NOT NULL,
    closed_at INTEGER,
    PRIMARY KEY (commission_id, criterion_id, mandate_revision),
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS verification_recoveries (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    source_evidence_id TEXT NOT NULL UNIQUE REFERENCES evidence(id),
    mandate_revision INTEGER NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('rework', 'retry', 'reroute', 'escalate', 'block')),
    status TEXT NOT NULL CHECK (status IN ('pending', 'scheduled', 'attention_required', 'blocked', 'resolved')),
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS completion_briefings (
    commission_id TEXT PRIMARY KEY REFERENCES commissions(id),
    summary TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS blockers (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL UNIQUE REFERENCES assignments(id),
    code TEXT NOT NULL,
    requirement TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS operation_requests (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    worker_lease_id TEXT NOT NULL REFERENCES worker_leases(id),
    mandate_revision INTEGER NOT NULL,
    plan_revision INTEGER NOT NULL,
    operation TEXT NOT NULL,
    repository TEXT,
    target TEXT NOT NULL,
    parameters_json TEXT NOT NULL,
    destination TEXT,
    effect TEXT,
    consequences_json TEXT NOT NULL,
    limits_json TEXT NOT NULL,
    canonical_operation_json TEXT NOT NULL,
    operation_digest TEXT NOT NULL,
    classification TEXT NOT NULL CHECK (classification IN (
        'silent_journaled', 'non_blocking_notification', 'approval_gate', 'prohibited'
    )),
    status TEXT NOT NULL CHECK (status IN (
        'completed', 'approval_required', 'authorized', 'started', 'confirmed',
        'failed', 'uncertain', 'prohibited', 'revoked'
    )),
    classification_reason TEXT NOT NULL,
    proposed_at INTEGER NOT NULL,
    authorized_at INTEGER,
    started_at INTEGER,
    completed_at INTEGER,
    receipt_json TEXT,
    credential_process_id INTEGER,
    credential_process_marker TEXT,
    credential_process_status TEXT CHECK (credential_process_status IN ('active', 'contained'))
);

CREATE TABLE IF NOT EXISTS approval_gates (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    operation_request_id TEXT NOT NULL UNIQUE REFERENCES operation_requests(id),
    operation_digest TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'open', 'authorized', 'consumed', 'invalidated', 'revoked'
    )),
    opened_at INTEGER NOT NULL,
    authorized_at INTEGER,
    consumed_at INTEGER,
    invalidated_at INTEGER
);

CREATE TABLE IF NOT EXISTS operation_execution_identities (
    operation_request_id TEXT PRIMARY KEY REFERENCES operation_requests(id),
    idempotency_key TEXT NOT NULL UNIQUE,
    request_hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS credential_grants (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    worker_lease_id TEXT NOT NULL REFERENCES worker_leases(id),
    mandate_revision INTEGER NOT NULL,
    plan_revision INTEGER NOT NULL,
    credential_reference TEXT NOT NULL,
    capability TEXT NOT NULL,
    destination TEXT NOT NULL,
    exposure TEXT NOT NULL CHECK (exposure IN ('brokered_only', 'one_shot')),
    credential_expires_at INTEGER NOT NULL,
    revocation TEXT NOT NULL CHECK (revocation IN ('delete_from_keychain')),
    status TEXT NOT NULL CHECK (status IN ('active', 'consumed', 'revoked', 'expired')),
    created_at INTEGER NOT NULL,
    consumed_at INTEGER,
    revoked_at INTEGER
);

CREATE TABLE IF NOT EXISTS credential_exposure_grants (
    id TEXT PRIMARY KEY,
    credential_grant_id TEXT NOT NULL REFERENCES credential_grants(id),
    operation_request_id TEXT NOT NULL UNIQUE REFERENCES operation_requests(id),
    operation_digest TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('authorized', 'consumed', 'revoked')),
    authorized_at INTEGER NOT NULL,
    consumed_at INTEGER,
    revoked_at INTEGER
);

CREATE TABLE IF NOT EXISTS commission_amendments (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    base_revision INTEGER NOT NULL,
    authority_json TEXT NOT NULL,
    resource_ceilings_json TEXT NOT NULL,
    reason TEXT NOT NULL,
    diff_json TEXT NOT NULL,
    amendment_digest TEXT NOT NULL,
    impact_json TEXT NOT NULL,
    revalidation_json TEXT,
    status TEXT NOT NULL CHECK (status IN ('proposed', 'accepted', 'invalidated')),
    proposed_at INTEGER NOT NULL,
    accepted_at INTEGER
);

CREATE TABLE IF NOT EXISTS attachment_launch_tokens (
    token_hash TEXT PRIMARY KEY,
    expected_harness TEXT NOT NULL,
    expected_adapter_identity TEXT NOT NULL,
    expected_adapter_version TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    session_token_hash TEXT NOT NULL UNIQUE,
    harness TEXT NOT NULL,
    adapter_identity TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    native_session_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('full', 'limited', 'observer')),
    capabilities_json TEXT NOT NULL,
    missing_capabilities_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS commission_attachments (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    attachment_id TEXT NOT NULL REFERENCES attachments(id),
    role TEXT NOT NULL CHECK (role IN ('active', 'observer')),
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (commission_id, attachment_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS one_active_attachment_per_commission
ON commission_attachments (commission_id) WHERE role = 'active';

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    event_type TEXT NOT NULL CHECK (event_type IN (
        'commission_proposed', 'commission_accepted', 'commission_amended', 'assignment_ready',
        'plan_revised', 'attempt_started', 'resources_reserved', 'result_submitted',
        'result_accepted', 'result_integrated', 'reconciliation_required',
        'useful_concurrency_observed',
        'evidence_recorded',
        'commission_verified_complete', 'assignment_blocked',
        'attachment_joined', 'active_attachment_changed', 'attachment_capabilities_changed',
        'worker_steered', 'worker_interrupted', 'worker_activity',
        'commission_paused', 'commission_resumed', 'commission_cancelled',
        'recovery_decided', 'attempt_contained', 'restart_reconciled',
        'operation_classified', 'operation_notification', 'approval_gate_opened',
        'approval_gate_authorized', 'operation_started', 'operation_confirmed',
        'operation_failed', 'operation_uncertain', 'commission_amendment_proposed',
        'resource_ceiling_approaching', 'credential_grant_issued',
        'credential_grant_consumed', 'credential_exposure_authorized'
    )),
    commission_revision INTEGER NOT NULL,
    payload_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

fn upgrade_worker_commands(connection: &Connection) -> Result<(), TyrionError> {
    let schema = table_schema(connection, "worker_commands")?;
    if schema.is_none()
        || schema.as_deref().is_some_and(|definition| {
            definition.contains("'pending'") && definition.contains("idempotency_key")
        })
    {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        BEGIN IMMEDIATE;
        ALTER TABLE worker_commands RENAME TO worker_commands_before_outbox;
        CREATE TABLE worker_commands (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            worker_id TEXT NOT NULL REFERENCES workers(id),
            attachment_id TEXT NOT NULL REFERENCES attachments(id),
            kind TEXT NOT NULL CHECK (kind IN ('steer', 'interrupt')),
            payload_json TEXT NOT NULL,
            mandate_revision INTEGER NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('pending', 'delivered', 'failed')),
            idempotency_key TEXT NOT NULL UNIQUE,
            request_hash TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        INSERT INTO worker_commands (
            id, commission_id, worker_id, attachment_id, kind, payload_json,
            mandate_revision, status, idempotency_key, request_hash, created_at
        )
        SELECT id, commission_id, worker_id, attachment_id, kind, payload_json,
               mandate_revision, status, 'legacy:' || id, '', created_at
        FROM worker_commands_before_outbox;
        DROP TABLE worker_commands_before_outbox;
        COMMIT;
        "#,
    )?;
    Ok(())
}

pub(super) fn migrate(connection: &Connection) -> Result<(), TyrionError> {
    if !column_exists(connection, "commissions", "control_revision")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN control_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(connection, "commissions", "execution_json")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN execution_json TEXT NOT NULL DEFAULT '{\"kind\":\"deterministic\"}'",
            [],
        )?;
    }
    if !column_exists(connection, "commissions", "plan_json")? {
        connection.execute("ALTER TABLE commissions ADD COLUMN plan_json TEXT", [])?;
    }
    if !column_exists(connection, "commissions", "worker_requirements_json")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN worker_requirements_json TEXT NOT NULL DEFAULT '{}'",
            [],
        )?;
    }
    upgrade_commissions(connection)?;
    if !column_exists(connection, "commissions", "project_id")? {
        connection.execute("ALTER TABLE commissions ADD COLUMN project_id TEXT", [])?;
    }
    if !column_exists(connection, "commissions", "commission_constraints_json")? {
        connection.execute(
            "ALTER TABLE commissions ADD COLUMN commission_constraints_json TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    upgrade_profile_claims_for_inference(connection)?;
    if !column_exists(
        connection,
        "planned_assignments",
        "worker_requirements_json",
    )? {
        connection.execute(
            "ALTER TABLE planned_assignments ADD COLUMN worker_requirements_json TEXT NOT NULL DEFAULT '{}'",
            [],
        )?;
    }
    upgrade_criteria(connection)?;
    backfill_criterion_versions(connection)?;
    upgrade_assignments(connection)?;
    add_attempt_disposition(connection)?;
    upgrade_attempts(connection)?;
    add_attempt_timing_columns(connection)?;
    upgrade_workers(connection)?;
    upgrade_evidence(connection)?;
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS worker_leases (
            id TEXT PRIMARY KEY,
            attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
            issued_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            mandate_revision INTEGER NOT NULL DEFAULT 0,
            released_at INTEGER,
            status TEXT NOT NULL CHECK (status IN ('active', 'released', 'revoked', 'expired'))
        );
        "#,
    )?;
    if !column_exists(connection, "worker_leases", "mandate_revision")? {
        connection.execute(
            "ALTER TABLE worker_leases ADD COLUMN mandate_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(connection, "operation_requests", "credential_process_id")? {
        connection.execute(
            "ALTER TABLE operation_requests ADD COLUMN credential_process_id INTEGER",
            [],
        )?;
    }
    if !column_exists(
        connection,
        "operation_requests",
        "credential_process_marker",
    )? {
        connection.execute(
            "ALTER TABLE operation_requests ADD COLUMN credential_process_marker TEXT",
            [],
        )?;
    }
    if !column_exists(
        connection,
        "operation_requests",
        "credential_process_status",
    )? {
        connection.execute(
            "ALTER TABLE operation_requests ADD COLUMN credential_process_status TEXT CHECK (credential_process_status IN ('active', 'contained'))",
            [],
        )?;
    }
    add_result_columns(connection)?;
    upgrade_results(connection)?;
    let events_schema = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'events'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let needs_event_upgrade = events_schema.as_deref().is_some_and(|schema| {
        !schema.contains("active_attachment_changed")
            || !schema.contains("payload_json")
            || !schema.contains("result_integrated")
            || !schema.contains("commission_amended")
            || !schema.contains("useful_concurrency_observed")
            || !schema.contains("worker_steered")
            || !schema.contains("commission_paused")
            || !schema.contains("operation_classified")
            || !schema.contains("operation_notification")
            || !schema.contains("approval_gate_opened")
            || !schema.contains("operation_confirmed")
            || !schema.contains("commission_amendment_proposed")
            || !schema.contains("resource_ceiling_approaching")
            || !schema.contains("credential_grant_issued")
            || !schema.contains("credential_grant_consumed")
            || !schema.contains("credential_exposure_authorized")
            || !schema.contains("attachment_capabilities_changed")
    });
    if needs_event_upgrade {
        let payload_projection = if events_schema
            .as_deref()
            .is_some_and(|schema| schema.contains("payload_json"))
        {
            "payload_json"
        } else {
            "'{}'"
        };
        connection.execute_batch(&format!(
            r#"
            BEGIN IMMEDIATE;
            ALTER TABLE events RENAME TO events_before_parallelism;
            CREATE TABLE events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                commission_id TEXT NOT NULL REFERENCES commissions(id),
                event_type TEXT NOT NULL CHECK (event_type IN (
                    'commission_proposed', 'commission_accepted', 'commission_amended', 'assignment_ready',
                    'plan_revised', 'attempt_started', 'resources_reserved', 'result_submitted',
                    'result_accepted', 'result_integrated', 'reconciliation_required',
                    'useful_concurrency_observed',
                    'evidence_recorded',
                    'commission_verified_complete', 'assignment_blocked',
                    'attachment_joined', 'active_attachment_changed', 'attachment_capabilities_changed',
                    'worker_steered', 'worker_interrupted', 'worker_activity',
                    'commission_paused', 'commission_resumed', 'commission_cancelled',
                    'recovery_decided', 'attempt_contained', 'restart_reconciled',
                    'operation_classified', 'operation_notification', 'approval_gate_opened',
                    'approval_gate_authorized', 'operation_started', 'operation_confirmed',
                    'operation_failed', 'operation_uncertain', 'commission_amendment_proposed',
                    'resource_ceiling_approaching', 'credential_grant_issued',
                    'credential_grant_consumed', 'credential_exposure_authorized'
                )),
                commission_revision INTEGER NOT NULL,
                payload_json TEXT NOT NULL DEFAULT '{{}}',
                created_at INTEGER NOT NULL
            );
            INSERT INTO events (
                sequence, commission_id, event_type, commission_revision, payload_json, created_at
            )
            SELECT sequence, commission_id, event_type, commission_revision, {payload_projection}, created_at
            FROM events_before_parallelism;
            DROP TABLE events_before_parallelism;
            COMMIT;
            "#
        ))?;
    }
    backfill_planned_assignments(connection)?;
    backfill_assignment_metadata(connection)?;
    upgrade_worker_commands(connection)?;
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS credential_grants (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            assignment_id TEXT NOT NULL REFERENCES assignments(id),
            attempt_id TEXT NOT NULL REFERENCES attempts(id),
            worker_lease_id TEXT NOT NULL REFERENCES worker_leases(id),
            mandate_revision INTEGER NOT NULL,
            plan_revision INTEGER NOT NULL,
            credential_reference TEXT NOT NULL,
            capability TEXT NOT NULL,
            destination TEXT NOT NULL,
            exposure TEXT NOT NULL CHECK (exposure IN ('brokered_only', 'one_shot')),
            credential_expires_at INTEGER NOT NULL,
            revocation TEXT NOT NULL CHECK (revocation IN ('delete_from_keychain')),
            status TEXT NOT NULL CHECK (status IN ('active', 'consumed', 'revoked', 'expired')),
            created_at INTEGER NOT NULL,
            consumed_at INTEGER,
            revoked_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS credential_exposure_grants (
            id TEXT PRIMARY KEY,
            credential_grant_id TEXT NOT NULL REFERENCES credential_grants(id),
            operation_request_id TEXT NOT NULL UNIQUE REFERENCES operation_requests(id),
            operation_digest TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('authorized', 'consumed', 'revoked')),
            authorized_at INTEGER NOT NULL,
            consumed_at INTEGER,
            revoked_at INTEGER
        );
        "#,
    )?;
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS learning_observations (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            project_id TEXT NOT NULL,
            attachment_id TEXT NOT NULL REFERENCES attachments(id),
            claim_id TEXT REFERENCES profile_claims(id) ON DELETE SET NULL,
            statement TEXT NOT NULL,
            statement_fingerprint TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN (
                'principal_edit', 'explained_rejection',
                'unedited_acceptance', 'contradiction'
            )),
            explanation TEXT,
            strength TEXT NOT NULL CHECK (strength IN ('strong', 'weak', 'contradiction')),
            observed_at INTEGER NOT NULL,
            UNIQUE (commission_id, statement_fingerprint)
        );
        CREATE TABLE IF NOT EXISTS profile_claim_lifecycle (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            claim_id TEXT NOT NULL REFERENCES profile_claims(id) ON DELETE CASCADE,
            from_state TEXT,
            to_state TEXT NOT NULL CHECK (to_state IN (
                'candidate', 'active', 'suppressed', 'contradicted'
            )),
            reason TEXT NOT NULL,
            observation_id TEXT REFERENCES learning_observations(id),
            changed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS profile_claim_observations (
            claim_id TEXT NOT NULL REFERENCES profile_claims(id) ON DELETE CASCADE,
            observation_id TEXT NOT NULL REFERENCES learning_observations(id) ON DELETE CASCADE,
            PRIMARY KEY (claim_id, observation_id)
        );
        CREATE TABLE IF NOT EXISTS profile_claim_indexes (
            claim_id TEXT PRIMARY KEY REFERENCES profile_claims(id) ON DELETE CASCADE,
            statement_fingerprint TEXT NOT NULL,
            indexed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS profile_claim_caches (
            claim_id TEXT PRIMARY KEY REFERENCES profile_claims(id) ON DELETE CASCADE,
            cached_projection_json TEXT NOT NULL,
            cached_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS learning_boundaries (
            id TEXT PRIMARY KEY,
            scope_kind TEXT NOT NULL CHECK (scope_kind IN ('principal', 'project')),
            scope_id TEXT REFERENCES projects(project_id),
            statement_fingerprint TEXT NOT NULL,
            provenance_commission_id TEXT NOT NULL REFERENCES commissions(id),
            provenance_attachment_id TEXT NOT NULL REFERENCES attachments(id),
            created_at INTEGER NOT NULL,
            CHECK (
                (scope_kind = 'principal' AND scope_id IS NULL)
                OR (scope_kind = 'project' AND scope_id IS NOT NULL)
            ),
            UNIQUE (scope_kind, scope_id, statement_fingerprint)
        );
        CREATE TRIGGER IF NOT EXISTS learning_boundaries_principal_unique_insert
        BEFORE INSERT ON learning_boundaries
        WHEN NEW.scope_kind = 'principal' AND EXISTS (
            SELECT 1 FROM learning_boundaries
            WHERE scope_kind = 'principal'
              AND statement_fingerprint = NEW.statement_fingerprint
        )
        BEGIN
            SELECT RAISE(ABORT, 'Principal Learning Boundary already exists');
        END;
        CREATE TRIGGER IF NOT EXISTS learning_boundaries_principal_unique_update
        BEFORE UPDATE OF scope_kind, statement_fingerprint ON learning_boundaries
        WHEN NEW.scope_kind = 'principal' AND EXISTS (
            SELECT 1 FROM learning_boundaries
            WHERE scope_kind = 'principal'
              AND statement_fingerprint = NEW.statement_fingerprint
              AND id != OLD.id
        )
        BEGIN
            SELECT RAISE(ABORT, 'Principal Learning Boundary already exists');
        END;
        CREATE TABLE IF NOT EXISTS memory_deletion_receipts (
            id TEXT PRIMARY KEY,
            claim_id TEXT NOT NULL,
            scope_kind TEXT NOT NULL CHECK (scope_kind IN ('principal', 'project')),
            scope_id TEXT,
            cascade_json TEXT NOT NULL,
            remaining_related_claims_json TEXT NOT NULL,
            deleted_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS imported_commission_records (
            record_id TEXT PRIMARY KEY,
            project_id TEXT,
            record_json TEXT NOT NULL,
            checksum TEXT NOT NULL,
            imported_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS imported_profile_claim_attempts (
            claim_id TEXT NOT NULL REFERENCES profile_claims(id) ON DELETE CASCADE,
            attempt_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            record_json TEXT NOT NULL,
            imported_at INTEGER NOT NULL,
            PRIMARY KEY (claim_id, attempt_id)
        );
        CREATE TABLE IF NOT EXISTS memory_import_provenance (
            entity_kind TEXT NOT NULL CHECK (entity_kind IN ('claim_version', 'observation')),
            entity_id TEXT NOT NULL,
            entity_version INTEGER NOT NULL DEFAULT 0,
            provenance_json TEXT NOT NULL,
            PRIMARY KEY (entity_kind, entity_id, entity_version)
        );
        CREATE TABLE IF NOT EXISTS temporary_memory_materials (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            attempt_id TEXT REFERENCES attempts(id),
            result_id TEXT REFERENCES results(id),
            kind TEXT NOT NULL CHECK (kind IN ('raw_worker_transcript', 'unaccepted_artifact')),
            content_json TEXT,
            pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
            retained_by_evidence INTEGER NOT NULL DEFAULT 0 CHECK (retained_by_evidence IN (0, 1)),
            retained_by_claim INTEGER NOT NULL DEFAULT 0 CHECK (retained_by_claim IN (0, 1)),
            retained_for_uncertain_effect INTEGER NOT NULL DEFAULT 0 CHECK (
                retained_for_uncertain_effect IN (0, 1)
            ),
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            expired_at INTEGER
        );
        INSERT INTO profile_claim_lifecycle (
            claim_id, from_state, to_state, reason, changed_at
        )
        SELECT id, NULL, lifecycle_state, 'legacy_profile_migration', updated_at
        FROM profile_claims
        WHERE NOT EXISTS (
            SELECT 1 FROM profile_claim_lifecycle
            WHERE profile_claim_lifecycle.claim_id = profile_claims.id
        );
        INSERT OR IGNORE INTO profile_claim_observations (claim_id, observation_id)
        SELECT claim_id, id FROM learning_observations WHERE claim_id IS NOT NULL;
        INSERT OR IGNORE INTO profile_claim_indexes (
            claim_id, statement_fingerprint, indexed_at
        )
        SELECT id, statement_fingerprint, updated_at FROM profile_claims;
        INSERT OR IGNORE INTO profile_claim_caches (
            claim_id, cached_projection_json, cached_at
        )
        SELECT id, '{}', updated_at FROM profile_claims;
        "#,
    )?;
    connection.pragma_update(None, "user_version", 17)?;
    Ok(())
}

fn upgrade_profile_claims_for_inference(connection: &Connection) -> Result<(), TyrionError> {
    if !table_exists(connection, "profile_claims")? {
        return Ok(());
    }
    if !column_exists(connection, "profile_claims", "statement_fingerprint")? {
        connection.execute_batch(
            r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        ALTER TABLE profile_claims RENAME TO profile_claims_before_inference;
        CREATE TABLE profile_claims (
            id TEXT PRIMARY KEY,
            current_version INTEGER NOT NULL CHECK (current_version > 0),
            strength TEXT NOT NULL CHECK (strength IN ('hard', 'soft')),
            scope_kind TEXT NOT NULL CHECK (scope_kind IN ('principal', 'project')),
            scope_id TEXT REFERENCES projects(project_id),
            applicability TEXT NOT NULL CHECK (applicability = 'software_building'),
            confidence_category TEXT NOT NULL CHECK (confidence_category IN ('explicit', 'inferred')),
            confidence_basis_points INTEGER NOT NULL CHECK (
                confidence_basis_points >= 0 AND confidence_basis_points <= 10000
            ),
            lifecycle_state TEXT NOT NULL CHECK (lifecycle_state IN (
                'candidate', 'active', 'suppressed', 'contradicted'
            )),
            statement_fingerprint TEXT NOT NULL,
            last_nonweak_support_at INTEGER,
            lifecycle_changed_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            CHECK (
                (scope_kind = 'principal' AND scope_id IS NULL)
                OR (scope_kind = 'project' AND scope_id IS NOT NULL)
            ),
            FOREIGN KEY (id, current_version)
                REFERENCES profile_claim_versions(claim_id, version)
        );
        INSERT INTO profile_claims (
            id, current_version, strength, scope_kind, scope_id, applicability,
            confidence_category, confidence_basis_points, lifecycle_state,
            statement_fingerprint, last_nonweak_support_at, lifecycle_changed_at,
            created_at, updated_at
        )
        SELECT id, current_version, strength, scope_kind, scope_id, applicability,
               confidence_category, confidence_basis_points, lifecycle_state,
               'legacy:' || id, updated_at, updated_at, created_at, updated_at
        FROM profile_claims_before_inference;
        DROP TABLE profile_claims_before_inference;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
        )?;
    }
    backfill_legacy_profile_fingerprints(connection)
}

fn backfill_legacy_profile_fingerprints(connection: &Connection) -> Result<(), TyrionError> {
    let claims = {
        let mut statement = connection.prepare(
            "SELECT profile_claims.id, profile_claim_versions.statement
             FROM profile_claims
             JOIN profile_claim_versions
               ON profile_claim_versions.claim_id = profile_claims.id
              AND profile_claim_versions.version = profile_claims.current_version
             WHERE profile_claims.statement_fingerprint LIKE 'legacy:%'",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (claim_id, statement) in claims {
        connection.execute(
            "UPDATE profile_claims SET statement_fingerprint = ?2 WHERE id = ?1",
            rusqlite::params![claim_id, super::preference_fingerprint(&statement)],
        )?;
    }
    Ok(())
}

fn has_legacy_profile_fingerprints(connection: &Connection) -> Result<bool, TyrionError> {
    if !table_exists(connection, "profile_claims")?
        || !column_exists(connection, "profile_claims", "statement_fingerprint")?
    {
        return Ok(false);
    }
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM profile_claims
            WHERE statement_fingerprint LIKE 'legacy:%'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?)
}

pub(super) fn migration_required(connection: &Connection) -> Result<bool, TyrionError> {
    if !table_exists(connection, "commissions")? {
        return Ok(false);
    }
    let user_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    let events_schema = table_schema(connection, "events")?;
    let criteria_schema = table_schema(connection, "criteria")?;
    let assignments_schema = table_schema(connection, "assignments")?;
    let attempts_schema = table_schema(connection, "attempts")?;
    let evidence_schema = table_schema(connection, "evidence")?;
    let results_schema = table_schema(connection, "results")?;
    let commissions_schema = table_schema(connection, "commissions")?;
    let workers_schema = table_schema(connection, "workers")?;
    Ok(user_version < 17
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "commissions", "plan_json")?
        || !column_exists(connection, "commissions", "worker_requirements_json")?
        || !column_exists(connection, "commissions", "project_id")?
        || !column_exists(connection, "commissions", "commission_constraints_json")?
        || !table_exists(connection, "projects")?
        || !table_exists(connection, "project_identities")?
        || !table_exists(connection, "project_aliases")?
        || !table_exists(connection, "profile_claims")?
        || !table_exists(connection, "profile_claim_versions")?
        || !column_exists(connection, "profile_claims", "statement_fingerprint")?
        || has_legacy_profile_fingerprints(connection)?
        || !table_exists(connection, "learning_observations")?
        || !table_exists(connection, "profile_claim_lifecycle")?
        || !table_exists(connection, "profile_claim_observations")?
        || !table_exists(connection, "profile_claim_indexes")?
        || !table_exists(connection, "profile_claim_caches")?
        || !table_exists(connection, "learning_boundaries")?
        || !trigger_exists(connection, "learning_boundaries_principal_unique_insert")?
        || !trigger_exists(connection, "learning_boundaries_principal_unique_update")?
        || !table_exists(connection, "memory_deletion_receipts")?
        || !table_exists(connection, "imported_commission_records")?
        || !table_exists(connection, "imported_profile_claim_attempts")?
        || !table_exists(connection, "memory_import_provenance")?
        || !table_exists(connection, "temporary_memory_materials")?
        || !table_exists(connection, "attempt_context_packets")?
        || !table_exists(connection, "attempt_profile_claims")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || results_schema
            .as_ref()
            .is_some_and(|schema| !schema.contains("'superseded'"))
        || !table_exists(connection, "worker_leases")?
        || !column_exists(connection, "worker_leases", "mandate_revision")?
        || !table_exists(connection, "sandbox_cleanups")?
        || !table_exists(connection, "worker_configuration_failures")?
        || !table_exists(connection, "attempt_recoveries")?
        || !table_exists(connection, "restart_recoveries")?
        || !table_exists(connection, "watchdog_findings")?
        || !table_exists(connection, "operation_requests")?
        || !table_exists(connection, "approval_gates")?
        || !table_exists(connection, "operation_execution_identities")?
        || !table_exists(connection, "commission_amendments")?
        || !table_exists(connection, "credential_grants")?
        || !table_exists(connection, "credential_exposure_grants")?
        || !column_exists(connection, "operation_requests", "credential_process_id")?
        || !column_exists(
            connection,
            "operation_requests",
            "credential_process_marker",
        )?
        || !column_exists(
            connection,
            "operation_requests",
            "credential_process_status",
        )?
        || !table_exists(connection, "criterion_versions")?
        || !table_exists(connection, "verification_gates")?
        || !table_exists(connection, "verification_recoveries")?
        || !table_exists(connection, "commission_plans")?
        || !table_exists(connection, "planned_assignments")?
        || !column_exists(
            connection,
            "planned_assignments",
            "worker_requirements_json",
        )?
        || !table_exists(connection, "assignment_routes")?
        || !table_exists(connection, "assignment_skill_defaults")?
        || !table_exists(connection, "result_skill_executions")?
        || !table_exists(connection, "skill_associations")?
        || !table_exists(connection, "attention_conditions")?
        || !table_exists(connection, "workers")?
        || !table_exists(connection, "worker_commands")?
        || !column_exists(connection, "worker_commands", "idempotency_key")?
        || table_schema(connection, "worker_commands")?
            .is_some_and(|schema| !schema.contains("'pending'"))
        || !table_exists(connection, "assignment_metadata")?
        || !column_exists(connection, "assignment_metadata", "commission_id")?
        || !table_exists(connection, "resource_reservations")?
        || !column_exists(connection, "attempts", "started_at_ms")?
        || !column_exists(connection, "attempts", "execution_completed_at_ms")?
        || !column_exists(connection, "attempts", "revision_disposition")?
        || !column_exists(connection, "results", "revision_disposition")?
        || attempts_schema
            .as_ref()
            .is_some_and(|schema| !schema.contains("'requires_revalidation'"))
        || results_schema
            .as_ref()
            .is_some_and(|schema| !schema.contains("'requires_revalidation'"))
        || commissions_schema
            .is_some_and(|schema| !schema.contains("'paused'") || !schema.contains("'cancelled'"))
        || criteria_schema.is_some_and(|schema| !schema.contains("required_evidence"))
        || assignments_schema.is_some_and(|schema| {
            !schema.contains("'verification_pending'")
                || !schema.contains("'superseded'")
                || !schema.contains("'attention_required'")
                || !schema.contains("'cancelled'")
        })
        || attempts_schema.is_some_and(|schema| {
            !schema.contains("'failed'")
                || !schema.contains("'interrupted'")
                || !schema.contains("'timed_out'")
                || !schema.contains("'cancelled'")
        })
        || workers_schema.is_some_and(|schema| {
            !schema.contains("'timed_out'") || !schema.contains("'cancelled'")
        })
        || evidence_schema.is_some_and(|schema| !schema.contains("verification_attempt_id"))
        || events_schema.is_some_and(|schema| {
            !schema.contains("active_attachment_changed")
                || !schema.contains("payload_json")
                || !schema.contains("result_integrated")
                || !schema.contains("commission_amended")
                || !schema.contains("useful_concurrency_observed")
                || !schema.contains("worker_steered")
                || !schema.contains("commission_paused")
                || !schema.contains("operation_classified")
                || !schema.contains("operation_notification")
                || !schema.contains("approval_gate_opened")
                || !schema.contains("operation_confirmed")
                || !schema.contains("commission_amendment_proposed")
                || !schema.contains("resource_ceiling_approaching")
                || !schema.contains("credential_grant_issued")
                || !schema.contains("credential_grant_consumed")
                || !schema.contains("credential_exposure_authorized")
                || !schema.contains("attachment_capabilities_changed")
        }))
}

pub(super) fn migration_backup_path(database_path: &Path) -> Result<PathBuf, TyrionError> {
    let file_name = database_path
        .file_name()
        .ok_or_else(|| TyrionError::InvalidRequest("database path must have a file name".into()))?;
    let backup_name = format!("{}.pre-migration-v17", file_name.to_string_lossy());
    Ok(database_path.with_file_name(backup_name))
}

pub(super) fn create_backup(
    connection: &Connection,
    backup_path: &Path,
) -> Result<(), TyrionError> {
    if backup_path.exists() {
        return Err(TyrionError::InvalidRequest(format!(
            "refusing to overwrite pre-migration backup {}",
            backup_path.display()
        )));
    }
    let mut destination = Connection::open(backup_path)?;
    {
        let backup = Backup::new(connection, &mut destination)?;
        backup.run_to_completion(100, Duration::from_millis(10), None)?;
    }
    verify_integrity(&destination)?;
    Ok(())
}

pub(super) fn verify(connection: &Connection) -> Result<(), TyrionError> {
    verify_integrity(connection)?;
    let user_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if user_version != 17
        || !column_exists(connection, "commissions", "control_revision")?
        || !column_exists(connection, "commissions", "execution_json")?
        || !column_exists(connection, "commissions", "plan_json")?
        || !column_exists(connection, "commissions", "worker_requirements_json")?
        || !column_exists(connection, "commissions", "project_id")?
        || !column_exists(connection, "commissions", "commission_constraints_json")?
        || !table_exists(connection, "projects")?
        || !table_exists(connection, "project_identities")?
        || !table_exists(connection, "project_aliases")?
        || !table_exists(connection, "profile_claims")?
        || !table_exists(connection, "profile_claim_versions")?
        || !column_exists(connection, "profile_claims", "statement_fingerprint")?
        || has_legacy_profile_fingerprints(connection)?
        || !table_exists(connection, "learning_observations")?
        || !table_exists(connection, "profile_claim_lifecycle")?
        || !table_exists(connection, "profile_claim_observations")?
        || !table_exists(connection, "profile_claim_indexes")?
        || !table_exists(connection, "profile_claim_caches")?
        || !table_exists(connection, "learning_boundaries")?
        || !trigger_exists(connection, "learning_boundaries_principal_unique_insert")?
        || !trigger_exists(connection, "learning_boundaries_principal_unique_update")?
        || !table_exists(connection, "memory_deletion_receipts")?
        || !table_exists(connection, "imported_commission_records")?
        || !table_exists(connection, "imported_profile_claim_attempts")?
        || !table_exists(connection, "memory_import_provenance")?
        || !table_exists(connection, "temporary_memory_materials")?
        || !table_exists(connection, "attempt_context_packets")?
        || !table_exists(connection, "attempt_profile_claims")?
        || !column_exists(connection, "attachments", "session_token_hash")?
        || !column_exists(connection, "results", "integrated_artifact_revision")?
        || !table_exists(connection, "worker_leases")?
        || !column_exists(connection, "worker_leases", "mandate_revision")?
        || !table_exists(connection, "sandbox_cleanups")?
        || !table_exists(connection, "worker_configuration_failures")?
        || !table_exists(connection, "attempt_recoveries")?
        || !table_exists(connection, "restart_recoveries")?
        || !table_exists(connection, "watchdog_findings")?
        || !table_exists(connection, "operation_requests")?
        || !table_exists(connection, "approval_gates")?
        || !table_exists(connection, "operation_execution_identities")?
        || !table_exists(connection, "commission_amendments")?
        || !table_exists(connection, "credential_grants")?
        || !table_exists(connection, "credential_exposure_grants")?
        || !column_exists(connection, "operation_requests", "credential_process_id")?
        || !column_exists(
            connection,
            "operation_requests",
            "credential_process_marker",
        )?
        || !column_exists(
            connection,
            "operation_requests",
            "credential_process_status",
        )?
        || !table_exists(connection, "criterion_versions")?
        || !table_exists(connection, "verification_gates")?
        || !table_exists(connection, "verification_recoveries")?
        || !table_exists(connection, "commission_plans")?
        || !table_exists(connection, "planned_assignments")?
        || !column_exists(
            connection,
            "planned_assignments",
            "worker_requirements_json",
        )?
        || !table_exists(connection, "assignment_routes")?
        || !table_exists(connection, "assignment_skill_defaults")?
        || !table_exists(connection, "result_skill_executions")?
        || !table_exists(connection, "skill_associations")?
        || !table_exists(connection, "attention_conditions")?
        || !table_exists(connection, "workers")?
        || !table_exists(connection, "worker_commands")?
        || !column_exists(connection, "worker_commands", "idempotency_key")?
        || !table_exists(connection, "assignment_metadata")?
        || !column_exists(connection, "assignment_metadata", "commission_id")?
        || !table_exists(connection, "resource_reservations")?
        || !column_exists(connection, "attempts", "started_at_ms")?
        || !column_exists(connection, "attempts", "execution_completed_at_ms")?
        || !column_exists(connection, "attempts", "revision_disposition")?
        || !column_exists(connection, "results", "revision_disposition")?
    {
        return Err(TyrionError::InvalidRequest(
            "schema migration verification failed".into(),
        ));
    }
    let events_schema = table_schema(connection, "events")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create events".into())
    })?;
    let criteria_schema = table_schema(connection, "criteria")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create criteria".into())
    })?;
    let assignments_schema = table_schema(connection, "assignments")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create assignments".into())
    })?;
    let attempts_schema = table_schema(connection, "attempts")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create attempts".into())
    })?;
    let evidence_schema = table_schema(connection, "evidence")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create evidence".into())
    })?;
    let results_schema = table_schema(connection, "results")?.ok_or_else(|| {
        TyrionError::InvalidRequest("schema migration did not create results".into())
    })?;
    if !events_schema.contains("active_attachment_changed")
        || !events_schema.contains("payload_json")
        || !events_schema.contains("result_integrated")
        || !events_schema.contains("commission_amended")
        || !events_schema.contains("useful_concurrency_observed")
        || !events_schema.contains("worker_steered")
        || !events_schema.contains("commission_paused")
        || !events_schema.contains("operation_classified")
        || !events_schema.contains("operation_notification")
        || !events_schema.contains("approval_gate_opened")
        || !events_schema.contains("operation_confirmed")
        || !events_schema.contains("commission_amendment_proposed")
        || !events_schema.contains("resource_ceiling_approaching")
        || !events_schema.contains("credential_grant_issued")
        || !events_schema.contains("credential_grant_consumed")
        || !events_schema.contains("credential_exposure_authorized")
        || !events_schema.contains("attachment_capabilities_changed")
    {
        return Err(TyrionError::InvalidRequest(
            "events schema migration verification failed".into(),
        ));
    }
    if !criteria_schema.contains("required_evidence")
        || !criteria_schema.contains("'prompt'")
        || !assignments_schema.contains("'verification_pending'")
        || !assignments_schema.contains("'superseded'")
        || !assignments_schema.contains("'attention_required'")
        || !assignments_schema.contains("'cancelled'")
        || !attempts_schema.contains("'failed'")
        || !attempts_schema.contains("'interrupted'")
        || !attempts_schema.contains("'timed_out'")
        || !attempts_schema.contains("'cancelled'")
        || !attempts_schema.contains("'requires_revalidation'")
        || !results_schema.contains("'superseded'")
        || !results_schema.contains("'requires_revalidation'")
        || !evidence_schema.contains("verification_attempt_id")
    {
        return Err(TyrionError::InvalidRequest(
            "lifecycle schema migration verification failed".into(),
        ));
    }
    Ok(())
}

fn upgrade_criteria(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "criteria")? else {
        return Ok(());
    };
    if schema.contains("required_evidence") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE criteria_v5 (
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            description TEXT NOT NULL,
            required_evidence TEXT NOT NULL,
            verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
            verification_depth TEXT NOT NULL CHECK (verification_depth IN ('standard', 'independent')),
            verifier_configuration TEXT NOT NULL,
            verification_environment TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
            expected TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('uncertain', 'passed', 'failed')),
            PRIMARY KEY (commission_id, criterion_id)
        );
        INSERT INTO criteria_v5 (
            commission_id, criterion_id, position, description, required_evidence,
            verifier_type, verification_depth, verifier_configuration,
            verification_environment, verifier_kind, expected, status
        )
        SELECT commission_id, criterion_id, position, description, 'verifier_output',
               'deterministic', 'standard',
               CASE verifier_kind
                   WHEN 'exact_match' THEN 'deterministic-exact-match-v1'
                   ELSE 'contained-command-v1'
               END,
               'tyrion-controlled-v1', verifier_kind, expected,
               CASE status WHEN 'pending' THEN 'uncertain' ELSE status END
        FROM criteria;
        DROP TABLE criteria;
        ALTER TABLE criteria_v5 RENAME TO criteria;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn backfill_criterion_versions(connection: &Connection) -> Result<(), TyrionError> {
    connection.execute(
        "INSERT INTO criterion_versions (
            commission_id, mandate_revision, criterion_id, position, description,
            required_evidence, verifier_type, verification_depth,
            verifier_configuration, verification_environment, verifier_kind, expected
         )
         SELECT criteria.commission_id,
                CASE commissions.status
                    WHEN 'proposed' THEN 0
                    WHEN 'verified_complete' THEN commissions.revision - 1
                    ELSE commissions.revision
                END,
                criteria.criterion_id, criteria.position, criteria.description,
                criteria.required_evidence, criteria.verifier_type,
                criteria.verification_depth, criteria.verifier_configuration,
                criteria.verification_environment, criteria.verifier_kind, criteria.expected
         FROM criteria
         JOIN commissions ON commissions.id = criteria.commission_id
         WHERE NOT EXISTS (
             SELECT 1 FROM criterion_versions
             WHERE criterion_versions.commission_id = criteria.commission_id
         )",
        [],
    )?;
    Ok(())
}

fn upgrade_commissions(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "commissions")? else {
        return Ok(());
    };
    if schema.contains("'paused'") && schema.contains("'cancelled'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE commissions_v11 (
            id TEXT PRIMARY KEY,
            goal TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN (
                'proposed', 'active', 'paused', 'cancelled', 'verified_complete'
            )),
            revision INTEGER NOT NULL,
            control_revision INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            accepted_at INTEGER,
            completed_at INTEGER,
            artifact_revision TEXT,
            execution_json TEXT NOT NULL DEFAULT '{"kind":"deterministic"}',
            worker_requirements_json TEXT NOT NULL DEFAULT '{}',
            plan_json TEXT
        );
        INSERT INTO commissions_v11 SELECT * FROM commissions;
        DROP TABLE commissions;
        ALTER TABLE commissions_v11 RENAME TO commissions;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn upgrade_assignments(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "assignments")? else {
        return Ok(());
    };
    if schema.contains("'verification_pending'")
        && schema.contains("'superseded'")
        && schema.contains("'attention_required'")
        && schema.contains("'cancelled'")
    {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE assignments_v5 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            plan_revision INTEGER NOT NULL,
            status TEXT NOT NULL CHECK (status IN (
                'ready', 'running', 'accepted', 'superseded', 'verification_pending',
                'verification_failed', 'resource_blocked', 'attention_required', 'cancelled'
            )),
            created_at INTEGER NOT NULL,
            UNIQUE (id, commission_id)
        );
        INSERT INTO assignments_v5 SELECT * FROM assignments;
        DROP TABLE assignments;
        ALTER TABLE assignments_v5 RENAME TO assignments;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn upgrade_attempts(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "attempts")? else {
        return Ok(());
    };
    if schema.contains("'failed'")
        && schema.contains("'interrupted'")
        && schema.contains("'timed_out'")
        && schema.contains("'cancelled'")
        && schema.contains("revision_disposition")
    {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE attempts_v5 (
            id TEXT PRIMARY KEY,
            assignment_id TEXT NOT NULL REFERENCES assignments(id),
            worker_configuration TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN (
                'running', 'succeeded', 'failed', 'interrupted', 'timed_out', 'cancelled'
            )),
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            started_at_ms INTEGER NOT NULL,
            execution_completed_at_ms INTEGER,
            completed_at_ms INTEGER,
            revision_disposition TEXT NOT NULL DEFAULT 'current' CHECK (revision_disposition IN (
                'current', 'retained', 'superseded', 'stale', 'requires_revalidation'
            ))
        );
        INSERT INTO attempts_v5 (
            id, assignment_id, worker_configuration, status, started_at, completed_at,
            started_at_ms, execution_completed_at_ms, completed_at_ms, revision_disposition
        )
        SELECT id, assignment_id, worker_configuration, status, started_at, completed_at,
               started_at_ms, execution_completed_at_ms, completed_at_ms, revision_disposition
        FROM attempts;
        DROP TABLE attempts;
        ALTER TABLE attempts_v5 RENAME TO attempts;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn add_attempt_disposition(connection: &Connection) -> Result<(), TyrionError> {
    if !column_exists(connection, "attempts", "revision_disposition")? {
        connection.execute(
            "ALTER TABLE attempts ADD COLUMN revision_disposition TEXT NOT NULL DEFAULT 'current'",
            [],
        )?;
    }
    Ok(())
}

fn upgrade_workers(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "workers")? else {
        return Ok(());
    };
    if schema.contains("'timed_out'") && schema.contains("'cancelled'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE workers_v11 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            assignment_id TEXT NOT NULL REFERENCES assignments(id),
            attempt_id TEXT NOT NULL UNIQUE REFERENCES attempts(id),
            handle TEXT NOT NULL,
            configuration_json TEXT NOT NULL,
            routing_rationale_json TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN (
                'running', 'succeeded', 'failed', 'interrupted', 'timed_out', 'cancelled'
            )),
            native_session_id TEXT,
            latest_activity TEXT NOT NULL,
            activity_at_ms INTEGER NOT NULL,
            usage_json TEXT NOT NULL DEFAULT '{}',
            UNIQUE (commission_id, handle)
        );
        INSERT INTO workers_v11 SELECT * FROM workers;
        DROP TABLE workers;
        ALTER TABLE workers_v11 RENAME TO workers;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn add_attempt_timing_columns(connection: &Connection) -> Result<(), TyrionError> {
    if !column_exists(connection, "attempts", "started_at_ms")? {
        connection.execute(
            "ALTER TABLE attempts ADD COLUMN started_at_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        connection.execute(
            "UPDATE attempts SET started_at_ms = started_at * 1000 WHERE started_at_ms = 0",
            [],
        )?;
    }
    if !column_exists(connection, "attempts", "completed_at_ms")? {
        connection.execute(
            "ALTER TABLE attempts ADD COLUMN completed_at_ms INTEGER",
            [],
        )?;
        connection.execute(
            "UPDATE attempts SET completed_at_ms = completed_at * 1000 WHERE completed_at IS NOT NULL",
            [],
        )?;
    }
    if !column_exists(connection, "attempts", "execution_completed_at_ms")? {
        connection.execute(
            "ALTER TABLE attempts ADD COLUMN execution_completed_at_ms INTEGER",
            [],
        )?;
    }
    Ok(())
}

fn backfill_assignment_metadata(connection: &Connection) -> Result<(), TyrionError> {
    connection.execute(
        "INSERT INTO assignment_metadata (
            assignment_id, commission_id, logical_id, position, goal, purpose,
            read_scopes_json, write_scopes_json,
            concurrency_slots, max_storage_bytes, max_model_spend_cents,
            max_paid_service_spend_cents, legacy
         )
         SELECT assignments.id, assignments.commission_id, assignments.id, 0,
                commissions.goal, 'critical_path', '[]',
                COALESCE((
                    SELECT json_group_array(value) FROM authority_scopes
                    WHERE authority_scopes.commission_id = assignments.commission_id
                      AND scope_type = 'path'
                ), '[]'),
                1, resource_ceilings.max_storage_bytes,
                resource_ceilings.max_model_spend_cents,
                resource_ceilings.max_paid_service_spend_cents, 1
         FROM assignments
         JOIN commissions ON commissions.id = assignments.commission_id
         JOIN resource_ceilings ON resource_ceilings.commission_id = assignments.commission_id
         WHERE NOT EXISTS (
             SELECT 1 FROM assignment_metadata
             WHERE assignment_metadata.assignment_id = assignments.id
         )",
        [],
    )?;
    Ok(())
}

fn backfill_planned_assignments(connection: &Connection) -> Result<(), TyrionError> {
    connection.execute(
        "INSERT INTO planned_assignments (
            commission_id, logical_id, position, goal, purpose,
            read_scopes_json, write_scopes_json, concurrency_slots,
            max_storage_bytes, max_model_spend_cents,
            max_paid_service_spend_cents, created_plan_revision
         )
         SELECT assignments.commission_id, assignments.id, 0, commissions.goal,
                'critical_path', '[]',
                COALESCE((
                    SELECT json_group_array(value) FROM authority_scopes
                    WHERE authority_scopes.commission_id = assignments.commission_id
                      AND scope_type = 'path'
                ), '[]'),
                1, resource_ceilings.max_storage_bytes,
                resource_ceilings.max_model_spend_cents,
                resource_ceilings.max_paid_service_spend_cents,
                assignments.plan_revision
         FROM assignments
         JOIN commissions ON commissions.id = assignments.commission_id
         JOIN resource_ceilings ON resource_ceilings.commission_id = assignments.commission_id
         WHERE NOT EXISTS (
             SELECT 1 FROM assignment_metadata
             WHERE assignment_metadata.assignment_id = assignments.id
         )
           AND NOT EXISTS (
             SELECT 1 FROM planned_assignments
             WHERE planned_assignments.commission_id = assignments.commission_id
               AND planned_assignments.logical_id = assignments.id
         )",
        [],
    )?;
    Ok(())
}

fn upgrade_evidence(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "evidence")? else {
        return Ok(());
    };
    if schema.contains("verification_attempt_id") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE evidence_v5 (
            id TEXT PRIMARY KEY,
            commission_id TEXT NOT NULL REFERENCES commissions(id),
            criterion_id TEXT NOT NULL,
            result_id TEXT NOT NULL REFERENCES results(id),
            mandate_revision INTEGER NOT NULL,
            artifact_revision TEXT NOT NULL,
            evidence_type TEXT NOT NULL,
            verifier_type TEXT NOT NULL CHECK (verifier_type IN ('deterministic', 'model', 'principal')),
            scope TEXT NOT NULL CHECK (scope IN ('candidate', 'integrated', 'external')),
            verification_attempt_id TEXT NOT NULL,
            verifier_identity TEXT NOT NULL,
            verifier_configuration TEXT NOT NULL,
            verifier_kind TEXT NOT NULL CHECK (verifier_kind IN ('exact_match', 'command', 'prompt')),
            procedure_json TEXT NOT NULL,
            environment TEXT NOT NULL,
            outcome TEXT NOT NULL CHECK (outcome IN ('passed', 'failed', 'uncertain')),
            observed TEXT NOT NULL,
            expected TEXT NOT NULL,
            material_contradiction INTEGER NOT NULL CHECK (material_contradiction IN (0, 1)),
            defect TEXT CHECK (defect IN ('result', 'verifier', 'environment', 'criterion')),
            producer_attempt_id TEXT,
            created_at INTEGER NOT NULL,
            UNIQUE (commission_id, criterion_id, evidence_type, verification_attempt_id),
            FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
        );
        INSERT INTO evidence_v5 (
            id, commission_id, criterion_id, result_id, mandate_revision,
            artifact_revision, evidence_type, verifier_type, scope,
            verification_attempt_id, verifier_identity, verifier_configuration,
            verifier_kind, procedure_json, environment, outcome, observed,
            expected, material_contradiction, defect, producer_attempt_id, created_at
        )
        SELECT id, commission_id, criterion_id, result_id, mandate_revision,
               artifact_revision, 'verifier_output', 'deterministic', 'integrated',
               'legacy-' || id, verifier_kind, 'legacy-v4', verifier_kind,
               json_quote(expected), 'tyrion-controlled-v1', outcome, observed,
               expected, 0, CASE outcome WHEN 'failed' THEN 'result' ELSE NULL END,
               NULL, created_at
        FROM evidence;
        DROP TABLE evidence;
        ALTER TABLE evidence_v5 RENAME TO evidence;
        CREATE TRIGGER evidence_is_immutable_update
        BEFORE UPDATE ON evidence
        BEGIN
            SELECT RAISE(ABORT, 'Evidence is immutable');
        END;
        CREATE TRIGGER evidence_is_immutable_delete
        BEFORE DELETE ON evidence
        BEGIN
            SELECT RAISE(ABORT, 'Evidence is immutable');
        END;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn add_result_columns(connection: &Connection) -> Result<(), TyrionError> {
    let columns = [
        ("mandate_revision", "INTEGER"),
        ("plan_revision", "INTEGER"),
        ("base_revision", "TEXT"),
        ("candidate_commits_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("changed_paths_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("artifacts_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("verification_outcomes_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("known_effects_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("integrated_artifact_revision", "TEXT"),
        ("revision_disposition", "TEXT NOT NULL DEFAULT 'current'"),
    ];
    for (name, definition) in columns {
        if !column_exists(connection, "results", name)? {
            connection.execute(
                &format!("ALTER TABLE results ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn upgrade_results(connection: &Connection) -> Result<(), TyrionError> {
    let Some(schema) = table_schema(connection, "results")? else {
        return Ok(());
    };
    if schema.contains("'superseded'") && schema.contains("'requires_revalidation'") {
        return Ok(());
    }
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = OFF;
        BEGIN IMMEDIATE;
        CREATE TABLE results_v5 (
            id TEXT PRIMARY KEY,
            attempt_id TEXT NOT NULL REFERENCES attempts(id),
            output TEXT NOT NULL,
            artifact_revision TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'superseded')),
            created_at INTEGER NOT NULL,
            mandate_revision INTEGER,
            plan_revision INTEGER,
            base_revision TEXT,
            candidate_commits_json TEXT NOT NULL DEFAULT '[]',
            changed_paths_json TEXT NOT NULL DEFAULT '[]',
            artifacts_json TEXT NOT NULL DEFAULT '[]',
            verification_outcomes_json TEXT NOT NULL DEFAULT '[]',
            known_effects_json TEXT NOT NULL DEFAULT '[]',
            integrated_artifact_revision TEXT,
            revision_disposition TEXT NOT NULL DEFAULT 'current' CHECK (revision_disposition IN (
                'current', 'retained', 'superseded', 'stale', 'requires_revalidation'
            ))
        );
        INSERT INTO results_v5 (
            id, attempt_id, output, artifact_revision, status, created_at,
            mandate_revision, plan_revision, base_revision, candidate_commits_json,
            changed_paths_json, artifacts_json, verification_outcomes_json,
            known_effects_json, integrated_artifact_revision, revision_disposition
        )
        SELECT id, attempt_id, output, artifact_revision, status, created_at,
               mandate_revision, plan_revision, base_revision, candidate_commits_json,
               changed_paths_json, artifacts_json, verification_outcomes_json,
               known_effects_json, integrated_artifact_revision, revision_disposition
        FROM results;
        DROP TABLE results;
        ALTER TABLE results_v5 RENAME TO results;
        COMMIT;
        PRAGMA foreign_keys = ON;
        "#,
    )?;
    Ok(())
}

fn verify_integrity(connection: &Connection) -> Result<(), TyrionError> {
    let result =
        connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
    if result != "ok" {
        return Err(TyrionError::InvalidRequest(format!(
            "database integrity check failed: {result}"
        )));
    }
    let foreign_key_violation = connection
        .query_row(
            "SELECT \"table\", rowid, parent, fkid FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((table, rowid, parent, fkid)) = foreign_key_violation {
        return Err(TyrionError::InvalidRequest(format!(
            "foreign-key integrity failed for {table} row {rowid}, parent {parent}, key {fkid}"
        )));
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, TyrionError> {
    Ok(table_schema(connection, table)?.is_some())
}

fn trigger_exists(connection: &Connection, trigger: &str) -> Result<bool, TyrionError> {
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name = ?1
         )",
        [trigger],
        |row| row.get::<_, bool>(0),
    )?)
}

fn table_schema(connection: &Connection, table: &str) -> Result<Option<String>, TyrionError> {
    Ok(connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, TyrionError> {
    let exists = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2
         )",
        [table, column],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn store_open_uses_verified_temporary_backup() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("legacy database should open");
        connection
            .execute_batch(
                r#"
                CREATE TABLE commissions (
                    id TEXT PRIMARY KEY,
                    goal TEXT NOT NULL,
                    status TEXT NOT NULL,
                    revision INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    accepted_at INTEGER,
                    completed_at INTEGER,
                    artifact_revision TEXT
                );
                CREATE TABLE events (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    commission_id TEXT NOT NULL REFERENCES commissions(id),
                    event_type TEXT NOT NULL CHECK (event_type IN (
                        'commission_proposed', 'commission_accepted', 'assignment_ready',
                        'attempt_started', 'result_submitted', 'evidence_recorded',
                        'commission_verified_complete', 'assignment_blocked'
                    )),
                    commission_revision INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                PRAGMA user_version = 1;
                "#,
            )
            .expect("legacy schema should be created");
        assert!(migration_required(&connection).unwrap());
        drop(connection);

        let backup_path = migration_backup_path(&database_path).unwrap();
        fs::write(&backup_path, b"existing backup").unwrap();
        let error = super::super::Store::open(&database_path)
            .err()
            .expect("startup must refuse to overwrite a migration backup");
        assert!(error.to_string().contains("refusing to overwrite"));
        fs::remove_file(&backup_path).unwrap();

        drop(super::super::Store::open(&database_path).unwrap());
        assert!(!backup_path.exists());
        let connection = Connection::open(database_path).expect("migrated database should open");
        verify(&connection).unwrap();
        assert!(!migration_required(&connection).unwrap());
    }

    #[test]
    fn version_six_attempt_timing_survives_interrupted_status_migration() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("database should open");
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = OFF;
                DROP TABLE attempts;
                CREATE TABLE attempts (
                    id TEXT PRIMARY KEY,
                    assignment_id TEXT NOT NULL REFERENCES assignments(id),
                    worker_configuration TEXT NOT NULL,
                    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
                    started_at INTEGER NOT NULL,
                    completed_at INTEGER,
                    started_at_ms INTEGER NOT NULL,
                    execution_completed_at_ms INTEGER,
                    completed_at_ms INTEGER
                );
                INSERT INTO commissions (id, goal, status, revision, created_at)
                VALUES ('commission-v6', 'migration fixture', 'active', 1, 1);
                INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
                VALUES ('assignment-v6', 'commission-v6', 1, 'ready', 1);
                INSERT INTO attempts (
                    id, assignment_id, worker_configuration, status, started_at, completed_at,
                    started_at_ms, execution_completed_at_ms, completed_at_ms
                ) VALUES (
                    'attempt-v6', 'assignment-v6', 'legacy-worker', 'succeeded',
                    10, 20, 10001, 15002, 20003
                );
                PRAGMA user_version = 6;
                PRAGMA foreign_keys = ON;
                "#,
            )
            .unwrap();
        drop(connection);

        drop(super::super::Store::open(&database_path).unwrap());
        let connection = Connection::open(database_path).unwrap();
        let attempts_schema = table_schema(&connection, "attempts").unwrap().unwrap();
        assert!(attempts_schema.contains("'interrupted'"));
        assert!(column_exists(&connection, "attempts", "started_at_ms").unwrap());
        assert!(column_exists(&connection, "attempts", "execution_completed_at_ms").unwrap());
        assert!(column_exists(&connection, "attempts", "completed_at_ms").unwrap());
        let timing = connection
            .query_row(
                "SELECT started_at, completed_at, started_at_ms,
                        execution_completed_at_ms, completed_at_ms
                 FROM attempts WHERE id = 'attempt-v6'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(timing, (10, 20, 10001, 15002, 20003));
    }

    #[test]
    fn version_ten_results_gain_checked_dispositions_without_losing_plan_revision() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("database should open");
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = OFF;
                DROP TABLE results;
                CREATE TABLE results (
                    id TEXT PRIMARY KEY,
                    attempt_id TEXT NOT NULL REFERENCES attempts(id),
                    output TEXT NOT NULL,
                    artifact_revision TEXT NOT NULL,
                    status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'superseded')),
                    created_at INTEGER NOT NULL,
                    mandate_revision INTEGER,
                    plan_revision INTEGER,
                    base_revision TEXT,
                    candidate_commits_json TEXT NOT NULL DEFAULT '[]',
                    changed_paths_json TEXT NOT NULL DEFAULT '[]',
                    artifacts_json TEXT NOT NULL DEFAULT '[]',
                    verification_outcomes_json TEXT NOT NULL DEFAULT '[]',
                    known_effects_json TEXT NOT NULL DEFAULT '[]',
                    integrated_artifact_revision TEXT
                );
                INSERT INTO commissions (id, goal, status, revision, created_at)
                VALUES ('commission-v10', 'migration fixture', 'active', 1, 1);
                INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
                VALUES ('assignment-v10', 'commission-v10', 7, 'running', 1);
                INSERT INTO attempts (
                    id, assignment_id, worker_configuration, status, started_at,
                    started_at_ms, revision_disposition
                ) VALUES (
                    'attempt-v10', 'assignment-v10', 'legacy-worker', 'succeeded', 1, 1000, 'current'
                );
                INSERT INTO results (
                    id, attempt_id, output, artifact_revision, status, created_at,
                    mandate_revision, plan_revision
                ) VALUES (
                    'result-v10', 'attempt-v10', 'result', 'sha256:fixture',
                    'superseded', 2, 3, 7
                );
                PRAGMA user_version = 10;
                PRAGMA foreign_keys = ON;
                "#,
            )
            .unwrap();
        drop(connection);

        drop(super::super::Store::open(&database_path).unwrap());
        let connection = Connection::open(database_path).unwrap();
        let (plan_revision, disposition) = connection
            .query_row(
                "SELECT plan_revision, revision_disposition FROM results WHERE id = 'result-v10'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(plan_revision, 7);
        assert_eq!(disposition, "current");
        let invalid = connection.execute(
            "UPDATE results SET revision_disposition = 'invalid' WHERE id = 'result-v10'",
            [],
        );
        assert!(invalid.is_err());
    }

    #[test]
    fn version_fourteen_adds_immutable_skill_records() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("database should open");
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
                DROP TRIGGER assignment_skill_defaults_are_immutable_update;
                DROP TRIGGER assignment_skill_defaults_are_immutable_delete;
                DROP TABLE skill_associations;
                DROP TABLE result_skill_executions;
                DROP TABLE assignment_skill_defaults;
                PRAGMA user_version = 13;
                "#,
            )
            .unwrap();
        drop(connection);

        drop(super::super::Store::open(&database_path).unwrap());
        let connection = Connection::open(database_path).unwrap();
        verify(&connection).unwrap();
        assert!(table_exists(&connection, "assignment_skill_defaults").unwrap());
        assert!(table_exists(&connection, "result_skill_executions").unwrap());
        assert!(table_exists(&connection, "skill_associations").unwrap());
        connection
            .execute_batch(
                r#"
                INSERT INTO commissions (id, goal, status, revision, created_at)
                VALUES ('commission-v14', 'migration fixture', 'active', 1, 1);
                INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
                VALUES ('assignment-v14', 'commission-v14', 1, 'ready', 1);
                INSERT INTO assignment_skill_defaults (
                    assignment_id, skill_name, content_digest, requirement,
                    provenance, plan_revision, delegation, selected_at
                ) VALUES (
                    'assignment-v14', 'code-review',
                    'sha256:1111111111111111111111111111111111111111111111111111111111111111',
                    'required', 'principal', 1, 'native_unchanged', 1
                );
                "#,
            )
            .unwrap();
        let update = connection.execute(
            "UPDATE assignment_skill_defaults SET content_digest = 'changed'",
            [],
        );
        assert!(update.is_err());
    }

    #[test]
    fn version_fifteen_adds_learning_records_without_losing_commissions() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("database should open");
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
                INSERT INTO commissions (id, goal, status, revision, created_at)
                VALUES ('commission-v15', 'migration fixture', 'active', 1, 1);
                DROP TABLE attempt_profile_claims;
                DROP TABLE attempt_context_packets;
                DROP TRIGGER profile_claim_versions_are_immutable_update;
                DROP TABLE profile_claim_versions;
                DROP TABLE profile_claims;
                DROP TABLE project_aliases;
                DROP TABLE project_identities;
                DROP TABLE projects;
                ALTER TABLE commissions DROP COLUMN project_id;
                ALTER TABLE commissions DROP COLUMN commission_constraints_json;
                PRAGMA user_version = 14;
                "#,
            )
            .unwrap();
        drop(connection);

        drop(super::super::Store::open(&database_path).unwrap());
        let connection = Connection::open(database_path).unwrap();
        verify(&connection).unwrap();
        assert!(table_exists(&connection, "profile_claims").unwrap());
        assert!(table_exists(&connection, "profile_claim_versions").unwrap());
        assert!(table_exists(&connection, "projects").unwrap());
        assert!(table_exists(&connection, "project_identities").unwrap());
        assert!(table_exists(&connection, "project_aliases").unwrap());
        assert!(table_exists(&connection, "attempt_context_packets").unwrap());
        assert!(table_exists(&connection, "attempt_profile_claims").unwrap());
        let preserved = connection
            .query_row(
                "SELECT goal, project_id, commission_constraints_json
                 FROM commissions WHERE id = 'commission-v15'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(preserved, ("migration fixture".into(), None, "[]".into()));
    }

    #[test]
    fn version_sixteen_expands_profile_lifecycle_without_losing_hard_claims() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let database_path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&database_path).expect("database should open");
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
                DROP TABLE profile_claims;
                CREATE TABLE profile_claims (
                    id TEXT PRIMARY KEY,
                    current_version INTEGER NOT NULL CHECK (current_version > 0),
                    strength TEXT NOT NULL CHECK (strength = 'hard'),
                    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('principal', 'project')),
                    scope_id TEXT REFERENCES projects(project_id),
                    applicability TEXT NOT NULL CHECK (applicability = 'software_building'),
                    confidence_category TEXT NOT NULL CHECK (confidence_category = 'explicit'),
                    confidence_basis_points INTEGER NOT NULL CHECK (confidence_basis_points = 10000),
                    lifecycle_state TEXT NOT NULL CHECK (lifecycle_state IN ('active', 'suppressed')),
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    CHECK (
                        (scope_kind = 'principal' AND scope_id IS NULL)
                        OR (scope_kind = 'project' AND scope_id IS NOT NULL)
                    ),
                    FOREIGN KEY (id, current_version)
                        REFERENCES profile_claim_versions(claim_id, version)
                );
                INSERT INTO projects (project_id, created_at)
                VALUES ('project-v16', 1);
                INSERT INTO commissions (
                    id, goal, status, revision, control_revision, created_at, project_id
                ) VALUES (
                    'commission-v16', 'migration fixture', 'active', 1, 0, 1, 'project-v16'
                );
                INSERT INTO attachments (
                    id, session_token_hash, harness, adapter_identity, adapter_version,
                    protocol_version, native_session_id, mode, capabilities_json,
                    missing_capabilities_json, created_at
                ) VALUES (
                    'attachment-v16', 'fixture-token-hash', 'codex', 'fixture', '1',
                    2, 'fixture-session', 'full', '[]', '[]', 1
                );
                INSERT INTO profile_claim_versions (
                    claim_id, version, statement, token_upper_bound,
                    provenance_commission_id, provenance_attachment_id, created_at
                ) VALUES (
                    'claim-v16', 1, 'Prefer public seams.', 20,
                    'commission-v16', 'attachment-v16', 1
                );
                INSERT INTO profile_claims (
                    id, current_version, strength, scope_kind, scope_id, applicability,
                    confidence_category, confidence_basis_points, lifecycle_state,
                    created_at, updated_at
                ) VALUES (
                    'claim-v16', 1, 'hard', 'project', 'project-v16',
                    'software_building', 'explicit', 10000, 'active', 1, 1
                );
                PRAGMA user_version = 15;
                "#,
            )
            .unwrap();
        drop(connection);

        drop(super::super::Store::open(&database_path).unwrap());
        let connection = Connection::open(database_path).unwrap();
        verify(&connection).unwrap();
        let preserved = connection
            .query_row(
                "SELECT strength, lifecycle_state, statement_fingerprint,
                        last_nonweak_support_at, lifecycle_changed_at
                 FROM profile_claims WHERE id = 'claim-v16'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(preserved.0, "hard");
        assert_eq!(preserved.1, "active");
        assert_eq!(
            preserved.2,
            super::super::preference_fingerprint("Prefer public seams.")
        );
        assert_eq!(preserved.3, Some(1));
        assert_eq!(preserved.4, 1);
        assert!(table_exists(&connection, "learning_observations").unwrap());
        assert!(table_exists(&connection, "temporary_memory_materials").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM profile_claim_indexes WHERE claim_id = 'claim-v16'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM profile_claim_lifecycle
                     WHERE claim_id = 'claim-v16' AND reason = 'legacy_profile_migration'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );

        connection
            .execute(
                "UPDATE profile_claims
                 SET statement_fingerprint = 'legacy:claim-v16'
                 WHERE id = 'claim-v16'",
                [],
            )
            .unwrap();
        assert!(migration_required(&connection).unwrap());
        assert!(verify(&connection).is_err());
        migrate(&connection).unwrap();
        verify(&connection).unwrap();
        assert!(!migration_required(&connection).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT statement_fingerprint FROM profile_claims
                     WHERE id = 'claim-v16'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            super::super::preference_fingerprint("Prefer public seams.")
        );
    }
}

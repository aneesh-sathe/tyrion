use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    AssignmentStatus, AttemptStatus, AuthorityScopeType, CommissionStatus, CriterionStatus,
    EventKind, ResultStatus, WorkerLeaseStatus,
};
use crate::protocol::{
    AcceptanceCriterion, AdapterIdentity, AttachmentHandshake, CommissionProposal,
    CommissionReplayCursor, ExecutionSpec, Request, ResourceCeilings, VerificationAmendment,
    VerificationDefect, VerificationDepth, VerificationEvidenceSubmission, VerificationVerdict,
    Verifier, VerifierType, PROTOCOL_VERSION,
};
use crate::TyrionError;
use crate::{attachment, worker};

mod projection;
mod schema;

use projection::{event_value, inspect_commission as project_commission};

pub struct Store {
    connection: Connection,
}

struct ReadyAssignmentDispatch {
    assignment_id: String,
    goal: String,
    execution_json: String,
    mandate_revision: i64,
    commission_revision: i64,
    accepted_at: i64,
    max_attempts: u32,
    max_elapsed_seconds: u64,
    max_worker_concurrency: u32,
    max_storage_bytes: u64,
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

impl Store {
    pub fn open(database_path: &Path) -> Result<Self, TyrionError> {
        let connection = Connection::open(database_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let backup_path = if schema::migration_required(&connection)? {
            let backup_path = schema::migration_backup_path(database_path)?;
            schema::create_backup(&connection, &backup_path)?;
            Some(backup_path)
        } else {
            None
        };
        let migration = (|| {
            connection.execute_batch(schema::SCHEMA)?;
            schema::migrate(&connection)?;
            schema::verify(&connection)
        })();
        migration?;
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

        let commission_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO commissions (id, goal, status, revision, created_at, execution_json)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![
                commission_id,
                proposal.goal,
                CommissionStatus::Proposed.as_str(),
                unix_timestamp()?,
                serde_json::to_string(&proposal.execution)?,
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
    ) -> Result<Value, TyrionError> {
        let attachment_id = authenticated_attachment_id(&self.connection, request)?;
        ensure_commission_attachment(
            &self.connection,
            &attachment_id,
            commission_id,
            attachment::COMMISSION_INSPECTION,
        )?;
        project_commission(&self.connection, commission_id)
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
            "SELECT DISTINCT commission_id FROM assignments WHERE status = ?1 ORDER BY commission_id",
        )?;
        let rows = statement.query_map([AssignmentStatus::Ready.as_str()], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn accept_commission(
        &mut self,
        request: &Request,
        commission_id: &str,
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

        let assignment_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                assignment_id,
                commission_id,
                mandate_revision,
                AssignmentStatus::Ready.as_str(),
                accepted_at
            ],
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::AssignmentReady,
            mandate_revision,
        )?;

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
            "UPDATE results SET status = ?2
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
        let transaction = self.connection.transaction()?;
        let ready = transaction
            .query_row(
                "SELECT assignments.id, commissions.goal, assignments.plan_revision,
                        commissions.revision,
                        commissions.accepted_at, resource_ceilings.max_attempts,
                        resource_ceilings.max_elapsed_seconds,
                        resource_ceilings.max_worker_concurrency,
                        resource_ceilings.max_storage_bytes, commissions.execution_json
                 FROM assignments
                 JOIN commissions ON commissions.id = assignments.commission_id
                 JOIN resource_ceilings ON resource_ceilings.commission_id = commissions.id
                 WHERE assignments.commission_id = ?1
                   AND assignments.status = ?2
                   AND commissions.status = ?3
                 ORDER BY assignments.created_at, assignments.id
                 LIMIT 1",
                params![
                    commission_id,
                    AssignmentStatus::Ready.as_str(),
                    CommissionStatus::Active.as_str()
                ],
                |row| {
                    Ok(ReadyAssignmentDispatch {
                        assignment_id: row.get(0)?,
                        goal: row.get(1)?,
                        mandate_revision: row.get(2)?,
                        commission_revision: row.get(3)?,
                        accepted_at: row.get(4)?,
                        max_attempts: row.get(5)?,
                        max_elapsed_seconds: row.get(6)?,
                        max_worker_concurrency: row.get(7)?,
                        max_storage_bytes: row.get(8)?,
                        execution_json: row.get(9)?,
                    })
                },
            )
            .optional()?;
        let Some(ready) = ready else {
            return Ok(());
        };
        let execution: ExecutionSpec = serde_json::from_str(&ready.execution_json)?;
        if ready.mandate_revision != ready.commission_revision {
            return Err(TyrionError::InvalidRequest(format!(
                "ready Assignment {} is bound to mandate revision {}, but Commission {} is at revision {}",
                ready.assignment_id,
                ready.mandate_revision,
                commission_id,
                ready.commission_revision
            )));
        }

        let attempt_count = transaction.query_row(
            "SELECT COUNT(*) FROM attempts
             JOIN assignments ON assignments.id = attempts.assignment_id
             WHERE assignments.commission_id = ?1",
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
        let running_count = transaction.query_row(
            "SELECT COUNT(*) FROM attempts
             JOIN assignments ON assignments.id = attempts.assignment_id
             WHERE assignments.commission_id = ?1 AND attempts.status = ?2",
            params![commission_id, AttemptStatus::Running.as_str()],
            |row| row.get::<_, u32>(0),
        )?;
        if running_count >= ready.max_worker_concurrency {
            return Ok(());
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

        let criteria = load_criteria(&transaction, commission_id)?;
        let authorized_paths = load_authorized_paths(&transaction, commission_id)?;
        let configuration = worker.configuration(&execution)?;
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
        transaction.execute(
            "INSERT INTO attempts (id, assignment_id, worker_configuration, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                ready.assignment_id,
                configuration,
                AttemptStatus::Running.as_str(),
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO worker_leases (id, attempt_id, issued_at, expires_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                lease_id,
                attempt_id,
                now,
                lease_expires_at,
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
        record_event(
            &transaction,
            commission_id,
            EventKind::AttemptStarted,
            ready.mandate_revision,
        )?;
        transaction.commit()?;

        let assignment = worker::AssignmentContext {
            commission_id: commission_id.to_owned(),
            assignment_id: ready.assignment_id.clone(),
            attempt_id: attempt_id.clone(),
            mandate_revision: ready.mandate_revision,
            goal: ready.goal.clone(),
            execution,
            criteria,
            authorized_paths,
            max_storage_bytes: ready.max_storage_bytes,
            lease_expires_at,
        };
        let candidate = match worker.execute(&assignment) {
            Ok(candidate) => candidate,
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
        let transaction = self.connection.transaction()?;
        let current_revision = transaction.query_row(
            "SELECT revision FROM commissions WHERE id = ?1 AND status = ?2",
            params![commission_id, CommissionStatus::Active.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        if current_revision != ready.mandate_revision {
            return Err(TyrionError::InvalidRequest(format!(
                "Assignment {} Result targets mandate revision {}, but Commission {} is at revision {}",
                ready.assignment_id, ready.mandate_revision, commission_id, current_revision
            )));
        }
        let result_id = Uuid::new_v4().to_string();
        let result_created_at = unix_timestamp()?;
        let candidate_commits_json = serde_json::to_string(&candidate.candidate_commits)?;
        let changed_paths_json = serde_json::to_string(&candidate.changed_paths)?;
        let artifacts_json = serde_json::to_string(&candidate.artifacts)?;
        let known_effects_json = serde_json::to_string(&candidate.known_effects)?;
        transaction.execute(
            "INSERT INTO results (
                id, attempt_id, output, artifact_revision, status, created_at,
                mandate_revision, base_revision, candidate_commits_json,
                changed_paths_json, artifacts_json, known_effects_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                result_id,
                attempt_id,
                candidate.output,
                candidate.artifact_revision.as_str(),
                ResultStatus::Candidate.as_str(),
                result_created_at,
                ready.mandate_revision,
                candidate.base_revision,
                candidate_commits_json,
                changed_paths_json,
                artifacts_json,
                known_effects_json,
            ],
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::ResultSubmitted,
            ready.mandate_revision,
        )?;
        transaction.commit()?;

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
        let transaction = self.connection.transaction()?;
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
            finish_failed_verification(
                &transaction,
                &assignment.assignment_id,
                &attempt_id,
                &lease_id,
            )?;
            transaction.commit()?;
            return Ok(());
        }
        transaction.commit()?;

        let integrated = match worker.integrate(&assignment, &candidate) {
            Ok(integrated) => integrated,
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
        let mut artifacts = candidate.artifacts.clone();
        artifacts.extend(integrated.artifacts.clone());
        let transaction = self.connection.transaction()?;
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
        let transaction = self.connection.transaction()?;
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
            finish_failed_verification(
                &transaction,
                &assignment.assignment_id,
                &attempt_id,
                &lease_id,
            )?;
            transaction.commit()?;
            return Ok(());
        }

        let every_criterion_passed = transaction.query_row(
            "SELECT NOT EXISTS(
                SELECT 1 FROM criteria WHERE commission_id = ?1 AND status != ?2
             )",
            params![commission_id, CriterionStatus::Passed.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if !every_criterion_passed {
            finish_pending_verification(
                &transaction,
                &assignment.assignment_id,
                &attempt_id,
                &lease_id,
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
        let now = unix_timestamp()?;
        transaction.execute(
            "UPDATE attempts SET status = ?2, completed_at = ?3 WHERE id = ?1",
            params![attempt_id, AttemptStatus::Failed.as_str(), now],
        )?;
        let (lease_status, assignment_status, blocker_code, requirement) = match error {
            TyrionError::WorkerLeaseExpired { .. } => (
                WorkerLeaseStatus::Expired,
                AssignmentStatus::VerificationFailed,
                "worker_execution_failed".to_owned(),
                error.to_string(),
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
            ),
            _ => (
                WorkerLeaseStatus::Revoked,
                AssignmentStatus::VerificationFailed,
                "worker_execution_failed".to_owned(),
                error.to_string(),
            ),
        };
        transaction.execute(
            "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
            params![lease_id, lease_status.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![assignment_id, assignment_status.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO blockers (id, commission_id, assignment_id, code, requirement, created_at)
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
        record_event(
            &transaction,
            commission_id,
            EventKind::AssignmentBlocked,
            mandate_revision,
        )?;
        transaction.commit()?;
        Ok(())
    }
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

fn load_authorized_paths(
    transaction: &Transaction<'_>,
    commission_id: &str,
) -> Result<Vec<String>, TyrionError> {
    let mut statement = transaction.prepare(
        "SELECT value FROM authority_scopes
         WHERE commission_id = ?1 AND scope_type = ?2 ORDER BY position",
    )?;
    let rows = statement.query_map(
        params![commission_id, AuthorityScopeType::Path.as_str()],
        |row| row.get(0),
    )?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
            "Verified Completion requires passed current criteria, closed gates, and no material contradiction"
                .into(),
        ));
    }

    let completed_at = unix_timestamp()?;
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
    record_event(
        transaction,
        commission_id,
        EventKind::ResultAccepted,
        mandate_revision,
    )?;
    match (attempt_id, lease_id) {
        (Some(attempt_id), Some(lease_id)) => {
            transaction.execute(
                "UPDATE attempts SET status = ?2, completed_at = ?3 WHERE id = ?1",
                params![attempt_id, AttemptStatus::Succeeded.as_str(), completed_at],
            )?;
            transaction.execute(
                "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
                params![lease_id, WorkerLeaseStatus::Released.as_str(), completed_at],
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
    let completion_revision = mandate_revision + 1;
    let completed_commissions = transaction.execute(
        "UPDATE commissions
         SET status = ?2, revision = ?3, completed_at = ?4, artifact_revision = ?5
         WHERE id = ?1 AND status = ?6 AND revision = ?7",
        params![
            commission_id,
            CommissionStatus::VerifiedComplete.as_str(),
            completion_revision,
            completed_at,
            artifact_revision,
            CommissionStatus::Active.as_str(),
            mandate_revision,
        ],
    )?;
    if completed_commissions != 1 {
        return Err(TyrionError::StaleRevision {
            expected: mandate_revision,
            actual: transaction.query_row(
                "SELECT revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| row.get(0),
            )?,
        });
    }
    transaction.execute(
        "INSERT INTO completion_briefings (commission_id, summary, artifact_revision, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            commission_id,
            format!("Verified Complete: {goal}"),
            artifact_revision,
            completed_at,
        ],
    )?;
    record_event(
        transaction,
        commission_id,
        EventKind::CommissionVerifiedComplete,
        completion_revision,
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
                 WHERE counted.assignment_id = assignments.id),
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

fn finish_failed_verification(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    attempt_id: &str,
    lease_id: &str,
) -> Result<(), TyrionError> {
    let now = unix_timestamp()?;
    transaction.execute(
        "UPDATE attempts SET status = ?2, completed_at = ?3 WHERE id = ?1",
        params![attempt_id, AttemptStatus::Succeeded.as_str(), now],
    )?;
    transaction.execute(
        "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
        params![lease_id, WorkerLeaseStatus::Released.as_str(), now],
    )?;
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![assignment_id, AssignmentStatus::VerificationFailed.as_str()],
    )?;
    Ok(())
}

fn finish_pending_verification(
    transaction: &Transaction<'_>,
    assignment_id: &str,
    attempt_id: &str,
    lease_id: &str,
) -> Result<(), TyrionError> {
    let now = unix_timestamp()?;
    transaction.execute(
        "UPDATE attempts SET status = ?2, completed_at = ?3 WHERE id = ?1",
        params![attempt_id, AttemptStatus::Succeeded.as_str(), now],
    )?;
    transaction.execute(
        "UPDATE worker_leases SET status = ?2, released_at = ?3 WHERE id = ?1",
        params![lease_id, WorkerLeaseStatus::Released.as_str(), now],
    )?;
    transaction.execute(
        "UPDATE assignments SET status = ?2 WHERE id = ?1",
        params![
            assignment_id,
            AssignmentStatus::VerificationPending.as_str()
        ],
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
                    Some("commission_verified_complete" | "assignment_blocked")
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
    Ok(())
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

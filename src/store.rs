use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    AssignmentStatus, AttemptStatus, AuthorityScopeType, CommissionStatus, CriterionStatus,
    EventKind, EvidenceOutcome, ResultStatus,
};
use crate::protocol::{CommissionProposal, Request, ResourceCeilings, Verifier};
use crate::TyrionError;
use crate::{verification, worker};

mod projection;
mod schema;

use projection::inspect_commission;

pub struct Store {
    connection: Connection,
}

struct ReadyAssignmentDispatch {
    assignment_id: String,
    goal: String,
    mandate_revision: i64,
    commission_revision: i64,
    accepted_at: i64,
    max_attempts: u32,
    max_elapsed_seconds: u64,
    max_worker_concurrency: u32,
    max_storage_bytes: u64,
}

impl Store {
    pub fn open(database_path: &Path) -> Result<Self, TyrionError> {
        let connection = Connection::open(database_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(schema::SCHEMA)?;
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

        let commission_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO commissions (id, goal, status, revision, created_at) VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                commission_id,
                proposal.goal,
                CommissionStatus::Proposed.as_str(),
                unix_timestamp()?
            ],
        )?;

        for (position, criterion) in proposal.criteria.iter().enumerate() {
            let (verifier_kind, expected) = match &criterion.verifier {
                Verifier::ExactMatch { expected } => ("exact_match", expected),
            };
            transaction.execute(
                "INSERT INTO criteria (commission_id, criterion_id, position, description, verifier_kind, expected, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    commission_id,
                    criterion.id,
                    position as i64,
                    criterion.description,
                    verifier_kind,
                    expected,
                    CriterionStatus::Pending.as_str(),
                ],
            )?;
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

        let result = inspect_commission(&transaction, &commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn inspect_commission(&self, commission_id: &str) -> Result<Value, TyrionError> {
        inspect_commission(&self.connection, commission_id)
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
        if status != CommissionStatus::Proposed.as_str() {
            return Err(TyrionError::InvalidRequest(format!(
                "commission {commission_id} is already {}",
                status
            )));
        }
        let may_execute = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM authority_scopes
                WHERE commission_id = ?1 AND scope_type = ?2 AND value = ?3
             )",
            params![
                commission_id,
                AuthorityScopeType::Action.as_str(),
                worker::ACTION
            ],
            |row| row.get::<_, bool>(0),
        )?;
        if !may_execute {
            return Err(TyrionError::InvalidRequest(
                "the Authority Envelope does not permit deterministic.echo".into(),
            ));
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

        let result = inspect_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn run_ready_assignment(
        &mut self,
        commission_id: &str,
        worker: &dyn worker::Worker,
    ) -> Result<(), TyrionError> {
        let transaction = self.connection.transaction()?;
        let ready = transaction
            .query_row(
                "SELECT assignments.id, commissions.goal, assignments.plan_revision,
                        commissions.revision,
                        commissions.accepted_at, resource_ceilings.max_attempts,
                        resource_ceilings.max_elapsed_seconds,
                        resource_ceilings.max_worker_concurrency,
                        resource_ceilings.max_storage_bytes
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
                    })
                },
            )
            .optional()?;
        let Some(ready) = ready else {
            return Ok(());
        };
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
        if ready.goal.len() as u64 > ready.max_storage_bytes {
            return block_ready_assignment(
                transaction,
                commission_id,
                &ready.assignment_id,
                ready.mandate_revision,
                "max_storage_bytes",
                "Start a new Commission with a max_storage_bytes ceiling large enough for the Result.",
            );
        }

        let attempt_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO attempts (id, assignment_id, worker_configuration, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt_id,
                ready.assignment_id,
                worker.configuration(),
                AttemptStatus::Running.as_str(),
                now
            ],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = ?2 WHERE id = ?1",
            params![ready.assignment_id, AssignmentStatus::Running.as_str()],
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::AttemptStarted,
            ready.mandate_revision,
        )?;
        transaction.commit()?;

        let candidate = worker.execute(&ready.goal);
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
        transaction.execute(
            "INSERT INTO results (id, attempt_id, output, artifact_revision, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                result_id,
                attempt_id,
                candidate.output,
                candidate.artifact_revision,
                ResultStatus::Candidate.as_str(),
                result_created_at,
            ],
        )?;
        record_event(
            &transaction,
            commission_id,
            EventKind::ResultSubmitted,
            ready.mandate_revision,
        )?;

        let criteria = {
            let mut statement = transaction.prepare(
                "SELECT criterion_id, expected FROM criteria
                 WHERE commission_id = ?1 ORDER BY position",
            )?;
            let rows = statement.query_map([commission_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut every_criterion_passed = true;
        for (criterion_id, expected) in criteria {
            let verification = verification::exact_match(
                &expected,
                &candidate.output,
                &candidate.artifact_revision,
            );
            every_criterion_passed &= verification.outcome == EvidenceOutcome::Passed;
            let evidence_id = Uuid::new_v4().to_string();
            transaction.execute(
                "INSERT INTO evidence (
                    id, commission_id, criterion_id, result_id, mandate_revision,
                    artifact_revision, verifier_kind, outcome, observed, expected, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'exact_match', ?7, ?8, ?9, ?10)",
                params![
                    evidence_id,
                    commission_id,
                    criterion_id,
                    result_id,
                    ready.mandate_revision,
                    candidate.artifact_revision,
                    verification.outcome.as_str(),
                    verification.observed,
                    expected,
                    unix_timestamp()?,
                ],
            )?;
            transaction.execute(
                "UPDATE criteria SET status = ?3 WHERE commission_id = ?1 AND criterion_id = ?2",
                params![
                    commission_id,
                    criterion_id,
                    verification.outcome.criterion_status().as_str()
                ],
            )?;
            record_event(
                &transaction,
                commission_id,
                EventKind::EvidenceRecorded,
                ready.mandate_revision,
            )?;
        }

        transaction.execute(
            "UPDATE attempts SET status = ?2, completed_at = ?3 WHERE id = ?1",
            params![
                attempt_id,
                AttemptStatus::Succeeded.as_str(),
                unix_timestamp()?
            ],
        )?;
        if every_criterion_passed {
            transaction.execute(
                "UPDATE assignments SET status = ?2 WHERE id = ?1",
                params![ready.assignment_id, AssignmentStatus::Accepted.as_str()],
            )?;
            transaction.execute(
                "UPDATE results SET status = ?2 WHERE id = ?1",
                params![result_id, ResultStatus::Accepted.as_str()],
            )?;
            let completion_revision = ready.mandate_revision + 1;
            let completed_at = unix_timestamp()?;
            transaction.execute(
                "UPDATE commissions
                 SET status = ?2, revision = ?3, completed_at = ?4, artifact_revision = ?5
                 WHERE id = ?1",
                params![
                    commission_id,
                    CommissionStatus::VerifiedComplete.as_str(),
                    completion_revision,
                    completed_at,
                    candidate.artifact_revision,
                ],
            )?;
            transaction.execute(
                "INSERT INTO completion_briefings (commission_id, summary, artifact_revision, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    commission_id,
                    format!("Verified Complete: {}", ready.goal),
                    candidate.artifact_revision,
                    completed_at,
                ],
            )?;
            record_event(
                &transaction,
                commission_id,
                EventKind::CommissionVerifiedComplete,
                completion_revision,
            )?;
        } else {
            transaction.execute(
                "UPDATE assignments SET status = ?2 WHERE id = ?1",
                params![
                    ready.assignment_id,
                    AssignmentStatus::VerificationFailed.as_str()
                ],
            )?;
        }

        transaction.commit()?;
        Ok(())
    }
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

fn validate_proposal(proposal: &CommissionProposal) -> Result<(), TyrionError> {
    if proposal.goal.trim().is_empty() {
        return Err(TyrionError::InvalidRequest("goal must not be empty".into()));
    }
    if proposal.criteria.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "at least one acceptance criterion is required".into(),
        ));
    }
    let mut criterion_ids = HashSet::new();
    for criterion in &proposal.criteria {
        if criterion.id.trim().is_empty() || criterion.description.trim().is_empty() {
            return Err(TyrionError::InvalidRequest(
                "criterion id and description must not be empty".into(),
            ));
        }
        if !criterion_ids.insert(&criterion.id) {
            return Err(TyrionError::InvalidRequest(format!(
                "criterion id {} is duplicated",
                criterion.id
            )));
        }
    }
    if proposal.resource_ceilings.max_attempts == 0
        || proposal.resource_ceilings.max_elapsed_seconds == 0
        || proposal.resource_ceilings.max_worker_concurrency == 0
        || proposal.resource_ceilings.max_storage_bytes == 0
    {
        return Err(TyrionError::InvalidRequest(
            "attempt, elapsed-time, concurrency, and storage ceilings must be positive".into(),
        ));
    }
    ensure_result_fits_storage_ceiling(
        &proposal.goal,
        proposal.resource_ceilings.max_storage_bytes,
    )?;
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
    transaction.execute(
        "INSERT INTO events (commission_id, event_type, commission_revision, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![commission_id, event_kind.as_str(), revision, unix_timestamp()?],
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

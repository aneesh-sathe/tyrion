use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::protocol::{CommissionProposal, Request, ResourceCeilings, Verifier};
use crate::TyrionError;
use crate::{verification, worker};

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(database_path: &Path) -> Result<Self, TyrionError> {
        let connection = Connection::open(database_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
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
            "INSERT INTO commissions (id, goal, status, revision, created_at) VALUES (?1, ?2, 'proposed', 0, ?3)",
            params![commission_id, proposal.goal, unix_timestamp()?],
        )?;

        for (position, criterion) in proposal.criteria.iter().enumerate() {
            let (verifier_kind, expected) = match &criterion.verifier {
                Verifier::ExactMatch { expected } => ("exact_match", expected),
            };
            transaction.execute(
                "INSERT INTO criteria (commission_id, criterion_id, position, description, verifier_kind, expected, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
                params![
                    commission_id,
                    criterion.id,
                    position as i64,
                    criterion.description,
                    verifier_kind,
                    expected,
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
        record_event(&transaction, &commission_id, "commission_proposed", 0)?;

        let result = inspect_commission(&transaction, &commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn inspect_commission(&self, commission_id: &str) -> Result<Value, TyrionError> {
        inspect_commission(&self.connection, commission_id)
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

        let commission = transaction
            .query_row(
                "SELECT goal, status, revision FROM commissions WHERE id = ?1",
                [commission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;
        if commission.2 != expected_revision {
            return Err(TyrionError::StaleRevision {
                expected: expected_revision,
                actual: commission.2,
            });
        }
        if commission.1 != "proposed" {
            return Err(TyrionError::InvalidRequest(format!(
                "commission {commission_id} is already {}",
                commission.1
            )));
        }
        let may_execute = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM authority_scopes
                WHERE commission_id = ?1 AND scope_type = 'action' AND value = 'deterministic.echo'
             )",
            [commission_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !may_execute {
            return Err(TyrionError::InvalidRequest(
                "the Authority Envelope does not permit deterministic.echo".into(),
            ));
        }

        let accepted_at = unix_timestamp()?;
        let mandate_revision = commission.2 + 1;
        transaction.execute(
            "UPDATE commissions SET status = 'active', revision = ?2, accepted_at = ?3 WHERE id = ?1",
            params![commission_id, mandate_revision, accepted_at],
        )?;
        record_event(
            &transaction,
            commission_id,
            "commission_accepted",
            mandate_revision,
        )?;

        let assignment_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO assignments (id, commission_id, plan_revision, status, created_at)
             VALUES (?1, ?2, ?3, 'ready', ?4)",
            params![assignment_id, commission_id, mandate_revision, accepted_at],
        )?;
        record_event(
            &transaction,
            commission_id,
            "assignment_ready",
            mandate_revision,
        )?;

        let attempt_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO attempts (id, assignment_id, worker_configuration, status, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![
                attempt_id,
                assignment_id,
                worker::CONFIGURATION,
                accepted_at
            ],
        )?;
        transaction.execute(
            "UPDATE assignments SET status = 'running' WHERE id = ?1",
            [&assignment_id],
        )?;
        record_event(
            &transaction,
            commission_id,
            "attempt_started",
            mandate_revision,
        )?;

        let candidate = worker::execute(&commission.0);
        let result_id = Uuid::new_v4().to_string();
        let result_created_at = unix_timestamp()?;
        transaction.execute(
            "INSERT INTO results (id, attempt_id, output, artifact_revision, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'candidate', ?5)",
            params![
                result_id,
                attempt_id,
                candidate.output,
                candidate.artifact_revision,
                result_created_at,
            ],
        )?;
        record_event(
            &transaction,
            commission_id,
            "result_submitted",
            mandate_revision,
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
            let verification = verification::exact_match(&expected, &candidate.output);
            every_criterion_passed &= verification.outcome == "passed";
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
                    mandate_revision,
                    candidate.artifact_revision,
                    verification.outcome,
                    verification.observed,
                    expected,
                    unix_timestamp()?,
                ],
            )?;
            transaction.execute(
                "UPDATE criteria SET status = ?3 WHERE commission_id = ?1 AND criterion_id = ?2",
                params![commission_id, criterion_id, verification.outcome],
            )?;
            record_event(
                &transaction,
                commission_id,
                "evidence_recorded",
                mandate_revision,
            )?;
        }

        transaction.execute(
            "UPDATE attempts SET status = 'succeeded', completed_at = ?2 WHERE id = ?1",
            params![attempt_id, unix_timestamp()?],
        )?;
        if every_criterion_passed {
            transaction.execute(
                "UPDATE assignments SET status = 'accepted' WHERE id = ?1",
                [&assignment_id],
            )?;
            transaction.execute(
                "UPDATE results SET status = 'accepted' WHERE id = ?1",
                [&result_id],
            )?;
            let completion_revision = mandate_revision + 1;
            let completed_at = unix_timestamp()?;
            transaction.execute(
                "UPDATE commissions
                 SET status = 'verified_complete', revision = ?2, completed_at = ?3, artifact_revision = ?4
                 WHERE id = ?1",
                params![
                    commission_id,
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
                    format!("Verified Complete: {}", commission.0),
                    candidate.artifact_revision,
                    completed_at,
                ],
            )?;
            record_event(
                &transaction,
                commission_id,
                "commission_verified_complete",
                completion_revision,
            )?;
        } else {
            transaction.execute(
                "UPDATE assignments SET status = 'verification_failed' WHERE id = ?1",
                [&assignment_id],
            )?;
        }

        let result = inspect_commission(&transaction, commission_id)?;
        save_idempotent_result(&transaction, idempotency_key, &request_hash, &result)?;
        transaction.commit()?;
        Ok(result)
    }
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
        ("repository", &proposal.authority.repositories),
        ("path", &proposal.authority.paths),
        ("action", &proposal.authority.actions),
        ("destination", &proposal.authority.destinations),
        ("effect", &proposal.authority.effects),
    ];
    for (scope_type, values) in scopes {
        for (position, value) in values.iter().enumerate() {
            transaction.execute(
                "INSERT INTO authority_scopes (commission_id, scope_type, position, value) VALUES (?1, ?2, ?3, ?4)",
                params![commission_id, scope_type, position as i64, value],
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
    event_type: &str,
    revision: i64,
) -> Result<(), TyrionError> {
    transaction.execute(
        "INSERT INTO events (commission_id, event_type, commission_revision, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![commission_id, event_type, revision, unix_timestamp()?],
    )?;
    Ok(())
}

fn inspect_commission(connection: &Connection, commission_id: &str) -> Result<Value, TyrionError> {
    let commission = connection
        .query_row(
            "SELECT id, goal, status, revision, accepted_at, completed_at, artifact_revision
             FROM commissions WHERE id = ?1",
            [commission_id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "goal": row.get::<_, String>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "revision": row.get::<_, i64>(3)?,
                    "accepted_at": row.get::<_, Option<i64>>(4)?,
                    "completed_at": row.get::<_, Option<i64>>(5)?,
                    "artifact_revision": row.get::<_, Option<String>>(6)?,
                }))
            },
        )
        .optional()?
        .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;

    let authority = json!({
        "repositories": scope_values(connection, commission_id, "repository")?,
        "paths": scope_values(connection, commission_id, "path")?,
        "actions": scope_values(connection, commission_id, "action")?,
        "destinations": scope_values(connection, commission_id, "destination")?,
        "effects": scope_values(connection, commission_id, "effect")?,
    });
    let resource_ceilings = connection.query_row(
        "SELECT max_attempts, max_elapsed_seconds, max_worker_concurrency, max_storage_bytes,
                max_model_spend_cents, max_paid_service_spend_cents
         FROM resource_ceilings WHERE commission_id = ?1",
        [commission_id],
        |row| {
            Ok(json!({
                "max_attempts": row.get::<_, u32>(0)?,
                "max_elapsed_seconds": row.get::<_, u64>(1)?,
                "max_worker_concurrency": row.get::<_, u32>(2)?,
                "max_storage_bytes": row.get::<_, u64>(3)?,
                "max_model_spend_cents": row.get::<_, u64>(4)?,
                "max_paid_service_spend_cents": row.get::<_, u64>(5)?,
            }))
        },
    )?;
    let criteria = query_values(
        connection,
        "SELECT criterion_id, description, verifier_kind, expected, status
         FROM criteria WHERE commission_id = ?1 ORDER BY position",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "description": row.get::<_, String>(1)?,
                "verifier": {
                    "kind": row.get::<_, String>(2)?,
                    "expected": row.get::<_, String>(3)?,
                },
                "status": row.get::<_, String>(4)?,
            }))
        },
    )?;
    let known_uncertainties = query_values(
        connection,
        "SELECT description FROM known_uncertainties WHERE commission_id = ?1 ORDER BY position",
        commission_id,
        |row| Ok(Value::String(row.get(0)?)),
    )?;

    let mut commission = commission;
    commission["authority"] = authority;
    commission["resource_ceilings"] = resource_ceilings;
    commission["known_uncertainties"] = Value::Array(known_uncertainties);

    let assignments = query_values(
        connection,
        "SELECT id, plan_revision, status, created_at
         FROM assignments WHERE commission_id = ?1 ORDER BY created_at, id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "plan_revision": row.get::<_, i64>(1)?,
                "status": row.get::<_, String>(2)?,
                "created_at": row.get::<_, i64>(3)?,
            }))
        },
    )?;
    let attempts = query_values(
        connection,
        "SELECT attempts.id, attempts.assignment_id, attempts.worker_configuration,
                attempts.status, attempts.started_at, attempts.completed_at
         FROM attempts
         JOIN assignments ON assignments.id = attempts.assignment_id
         WHERE assignments.commission_id = ?1 ORDER BY attempts.started_at, attempts.id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "worker_configuration": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "started_at": row.get::<_, i64>(4)?,
                "completed_at": row.get::<_, Option<i64>>(5)?,
            }))
        },
    )?;
    let results = query_values(
        connection,
        "SELECT results.id, results.attempt_id, results.output, results.artifact_revision,
                results.status, results.created_at
         FROM results
         JOIN attempts ON attempts.id = results.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         WHERE assignments.commission_id = ?1 ORDER BY results.created_at, results.id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "attempt_id": row.get::<_, String>(1)?,
                "output": row.get::<_, String>(2)?,
                "artifact_revision": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
                "created_at": row.get::<_, i64>(5)?,
            }))
        },
    )?;
    let evidence = query_values(
        connection,
        "SELECT id, criterion_id, result_id, mandate_revision, artifact_revision,
                verifier_kind, outcome, observed, expected, created_at
         FROM evidence WHERE commission_id = ?1 ORDER BY created_at, criterion_id",
        commission_id,
        evidence_value,
    )?;
    let briefing = completion_briefing(connection, commission_id)?;
    let events = query_values(
        connection,
        "SELECT sequence, event_type, commission_revision, created_at
         FROM events WHERE commission_id = ?1 ORDER BY sequence",
        commission_id,
        |row| {
            Ok(json!({
                "sequence": row.get::<_, i64>(0)?,
                "type": row.get::<_, String>(1)?,
                "commission_revision": row.get::<_, i64>(2)?,
                "created_at": row.get::<_, i64>(3)?,
            }))
        },
    )?;

    Ok(json!({
        "commission": commission,
        "criteria": criteria,
        "assignments": assignments,
        "attempts": attempts,
        "results": results,
        "evidence": evidence,
        "briefing": briefing,
        "events": events,
    }))
}

fn evidence_value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "criterion_id": row.get::<_, String>(1)?,
        "result_id": row.get::<_, String>(2)?,
        "mandate_revision": row.get::<_, i64>(3)?,
        "artifact_revision": row.get::<_, String>(4)?,
        "verifier_kind": row.get::<_, String>(5)?,
        "outcome": row.get::<_, String>(6)?,
        "observed": row.get::<_, String>(7)?,
        "expected": row.get::<_, String>(8)?,
        "created_at": row.get::<_, i64>(9)?,
    }))
}

fn completion_briefing(
    connection: &Connection,
    commission_id: &str,
) -> Result<Option<Value>, TyrionError> {
    let row = connection
        .query_row(
            "SELECT summary, artifact_revision FROM completion_briefings WHERE commission_id = ?1",
            [commission_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((summary, artifact_revision)) = row else {
        return Ok(None);
    };
    let criteria = query_values(
        connection,
        "SELECT criteria.criterion_id, criteria.description, criteria.status,
                evidence.id, evidence.result_id, evidence.mandate_revision,
                evidence.artifact_revision, evidence.verifier_kind, evidence.outcome,
                evidence.observed, evidence.expected, evidence.created_at
         FROM criteria
         JOIN evidence ON evidence.commission_id = criteria.commission_id
                      AND evidence.criterion_id = criteria.criterion_id
         WHERE criteria.commission_id = ?1 ORDER BY criteria.position",
        commission_id,
        |row| {
            Ok(json!({
                "criterion_id": row.get::<_, String>(0)?,
                "description": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "evidence": {
                    "id": row.get::<_, String>(3)?,
                    "result_id": row.get::<_, String>(4)?,
                    "mandate_revision": row.get::<_, i64>(5)?,
                    "artifact_revision": row.get::<_, String>(6)?,
                    "verifier_kind": row.get::<_, String>(7)?,
                    "outcome": row.get::<_, String>(8)?,
                    "observed": row.get::<_, String>(9)?,
                    "expected": row.get::<_, String>(10)?,
                    "created_at": row.get::<_, i64>(11)?,
                },
            }))
        },
    )?;
    let completion_revision = connection.query_row(
        "SELECT revision FROM commissions WHERE id = ?1",
        [commission_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(Some(json!({
        "title": "Verified Completion",
        "summary": summary,
        "commission_id": commission_id,
        "completion_revision": completion_revision,
        "artifact_revision": artifact_revision,
        "criteria": criteria,
    })))
}

fn scope_values(
    connection: &Connection,
    commission_id: &str,
    scope_type: &str,
) -> Result<Vec<String>, TyrionError> {
    let mut statement = connection.prepare(
        "SELECT value FROM authority_scopes
         WHERE commission_id = ?1 AND scope_type = ?2 ORDER BY position",
    )?;
    let rows = statement.query_map(params![commission_id, scope_type], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn query_values<F>(
    connection: &Connection,
    sql: &str,
    commission_id: &str,
    map: F,
) -> Result<Vec<Value>, TyrionError>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Value>,
{
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([commission_id], map)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn unix_timestamp() -> Result<i64, TyrionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            TyrionError::InvalidRequest(format!("system clock is invalid: {error}"))
        })?;
    Ok(duration.as_secs() as i64)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commissions (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('proposed', 'active', 'verified_complete')),
    revision INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    accepted_at INTEGER,
    completed_at INTEGER,
    artifact_revision TEXT
);

CREATE TABLE IF NOT EXISTS criteria (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    description TEXT NOT NULL,
    verifier_kind TEXT NOT NULL,
    expected TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'passed', 'failed')),
    PRIMARY KEY (commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS authority_scopes (
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    scope_type TEXT NOT NULL,
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

CREATE TABLE IF NOT EXISTS assignments (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    plan_revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attempts (
    id TEXT PRIMARY KEY,
    assignment_id TEXT NOT NULL REFERENCES assignments(id),
    worker_configuration TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    completed_at INTEGER
);

CREATE TABLE IF NOT EXISTS results (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    output TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    criterion_id TEXT NOT NULL,
    result_id TEXT NOT NULL REFERENCES results(id),
    mandate_revision INTEGER NOT NULL,
    artifact_revision TEXT NOT NULL,
    verifier_kind TEXT NOT NULL,
    outcome TEXT NOT NULL,
    observed TEXT NOT NULL,
    expected TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (commission_id, criterion_id) REFERENCES criteria(commission_id, criterion_id)
);

CREATE TABLE IF NOT EXISTS completion_briefings (
    commission_id TEXT PRIMARY KEY REFERENCES commissions(id),
    summary TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    commission_id TEXT NOT NULL REFERENCES commissions(id),
    event_type TEXT NOT NULL,
    commission_revision INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

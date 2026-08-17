use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::domain::AuthorityScopeType;
use crate::TyrionError;

pub(super) fn inspect_commission(
    connection: &Connection,
    commission_id: &str,
) -> Result<Value, TyrionError> {
    let commission = connection
        .query_row(
            "SELECT id, goal, status, revision, control_revision, accepted_at, completed_at, artifact_revision
             FROM commissions WHERE id = ?1",
            [commission_id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "goal": row.get::<_, String>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "revision": row.get::<_, i64>(3)?,
                    "control_revision": row.get::<_, i64>(4)?,
                    "accepted_at": row.get::<_, Option<i64>>(5)?,
                    "completed_at": row.get::<_, Option<i64>>(6)?,
                    "artifact_revision": row.get::<_, Option<String>>(7)?,
                }))
            },
        )
        .optional()?
        .ok_or_else(|| TyrionError::NotFound(commission_id.to_owned()))?;

    let authority = json!({
        "repositories": scope_values(connection, commission_id, AuthorityScopeType::Repository)?,
        "paths": scope_values(connection, commission_id, AuthorityScopeType::Path)?,
        "actions": scope_values(connection, commission_id, AuthorityScopeType::Action)?,
        "destinations": scope_values(connection, commission_id, AuthorityScopeType::Destination)?,
        "effects": scope_values(connection, commission_id, AuthorityScopeType::Effect)?,
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
        "SELECT sequence, event_type, commission_revision, payload_json, created_at
         FROM events WHERE commission_id = ?1 ORDER BY sequence",
        commission_id,
        event_value,
    )?;
    let blockers = query_values(
        connection,
        "SELECT id, assignment_id, code, requirement, created_at
         FROM blockers WHERE commission_id = ?1 ORDER BY created_at, id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "code": row.get::<_, String>(2)?,
                "requirement": row.get::<_, String>(3)?,
                "created_at": row.get::<_, i64>(4)?,
            }))
        },
    )?;
    let attachments = query_values(
        connection,
        "SELECT attachments.id, attachments.harness, attachments.adapter_identity,
                attachments.adapter_version, attachments.native_session_id, attachments.mode,
                commission_attachments.role, commission_attachments.joined_at
         FROM commission_attachments
         JOIN attachments ON attachments.id = commission_attachments.attachment_id
         WHERE commission_attachments.commission_id = ?1
         ORDER BY commission_attachments.joined_at, attachments.id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "harness": row.get::<_, String>(1)?,
                "adapter_identity": row.get::<_, String>(2)?,
                "adapter_version": row.get::<_, String>(3)?,
                "native_session_id": row.get::<_, String>(4)?,
                "mode": row.get::<_, String>(5)?,
                "role": row.get::<_, String>(6)?,
                "joined_at": row.get::<_, i64>(7)?,
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
        "blockers": blockers,
        "attachments": attachments,
    }))
}

pub(super) fn event_value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let payload_json = row.get::<_, String>(3)?;
    let payload = serde_json::from_str::<Value>(&payload_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(json!({
        "sequence": row.get::<_, i64>(0)?,
        "type": row.get::<_, String>(1)?,
        "commission_revision": row.get::<_, i64>(2)?,
        "payload": payload,
        "created_at": row.get::<_, i64>(4)?,
    }))
}

fn evidence_value(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    evidence_value_at(row, 0)
}

fn evidence_value_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(offset)?,
        "criterion_id": row.get::<_, String>(offset + 1)?,
        "result_id": row.get::<_, String>(offset + 2)?,
        "mandate_revision": row.get::<_, i64>(offset + 3)?,
        "artifact_revision": row.get::<_, String>(offset + 4)?,
        "verifier_kind": row.get::<_, String>(offset + 5)?,
        "outcome": row.get::<_, String>(offset + 6)?,
        "observed": row.get::<_, String>(offset + 7)?,
        "expected": row.get::<_, String>(offset + 8)?,
        "created_at": row.get::<_, i64>(offset + 9)?,
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
                evidence.id, evidence.criterion_id, evidence.result_id, evidence.mandate_revision,
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
                "evidence": evidence_value_at(row, 3)?,
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

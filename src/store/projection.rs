use std::collections::HashMap;

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
            "SELECT id, goal, status, revision, control_revision, accepted_at, completed_at,
                    artifact_revision, execution_json
             FROM commissions WHERE id = ?1",
            [commission_id],
            |row| {
                let execution = json_column(row, 8)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "goal": row.get::<_, String>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "revision": row.get::<_, i64>(3)?,
                    "control_revision": row.get::<_, i64>(4)?,
                    "accepted_at": row.get::<_, Option<i64>>(5)?,
                    "completed_at": row.get::<_, Option<i64>>(6)?,
                    "artifact_revision": row.get::<_, Option<String>>(7)?,
                    "execution": execution,
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
        "SELECT criterion_id, description, required_evidence, verifier_type,
                verification_depth, verifier_configuration, verification_environment,
                verifier_kind, expected, status
         FROM criteria WHERE commission_id = ?1 ORDER BY position",
        commission_id,
        |row| {
            let verifier_kind = row.get::<_, String>(7)?;
            let expected = row.get::<_, String>(8)?;
            let verifier = if verifier_kind == "command" {
                json!({"kind": "command", "argv": serde_json::from_str::<Value>(&expected).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?})
            } else if verifier_kind == "prompt" {
                json!({"kind": "prompt", "prompt": expected})
            } else {
                json!({"kind": verifier_kind, "expected": expected})
            };
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "description": row.get::<_, String>(1)?,
                "required_evidence": row.get::<_, String>(2)?,
                "verifier_type": row.get::<_, String>(3)?,
                "verification_depth": row.get::<_, String>(4)?,
                "verifier_configuration": row.get::<_, String>(5)?,
                "verification_environment": row.get::<_, String>(6)?,
                "verifier": verifier,
                "status": row.get::<_, String>(9)?,
            }))
        },
    )?;
    let criterion_versions = query_values(
        connection,
        "SELECT mandate_revision, criterion_id, description, required_evidence,
                verifier_type, verification_depth, verifier_configuration,
                verification_environment, verifier_kind, expected
         FROM criterion_versions
         WHERE commission_id = ?1
         ORDER BY mandate_revision, position",
        commission_id,
        |row| {
            let verifier_kind = row.get::<_, String>(8)?;
            let expected = row.get::<_, String>(9)?;
            let verifier = if verifier_kind == "command" {
                json!({"kind": "command", "argv": serde_json::from_str::<Value>(&expected).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        9,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?})
            } else if verifier_kind == "prompt" {
                json!({"kind": "prompt", "prompt": expected})
            } else {
                json!({"kind": verifier_kind, "expected": expected})
            };
            Ok(json!({
                "mandate_revision": row.get::<_, i64>(0)?,
                "id": row.get::<_, String>(1)?,
                "description": row.get::<_, String>(2)?,
                "required_evidence": row.get::<_, String>(3)?,
                "verifier_type": row.get::<_, String>(4)?,
                "verification_depth": row.get::<_, String>(5)?,
                "verifier_configuration": row.get::<_, String>(6)?,
                "verification_environment": row.get::<_, String>(7)?,
                "verifier": verifier,
            }))
        },
    )?;
    let verification_gates = query_values(
        connection,
        "SELECT verification_gates.criterion_id,
                verification_gates.mandate_revision, verification_gates.status,
                verification_gates.opened_at, verification_gates.closed_at,
                verification_gates.mandate_revision = CASE commissions.status
                    WHEN 'verified_complete' THEN commissions.revision - 1
                    ELSE commissions.revision
                END
         FROM verification_gates
         JOIN commissions ON commissions.id = verification_gates.commission_id
         WHERE verification_gates.commission_id = ?1
         ORDER BY verification_gates.mandate_revision, verification_gates.criterion_id",
        commission_id,
        |row| {
            Ok(json!({
                "criterion_id": row.get::<_, String>(0)?,
                "mandate_revision": row.get::<_, i64>(1)?,
                "status": row.get::<_, String>(2)?,
                "opened_at": row.get::<_, i64>(3)?,
                "closed_at": row.get::<_, Option<i64>>(4)?,
                "current": row.get::<_, bool>(5)?,
            }))
        },
    )?;
    let verification_recoveries = query_values(
        connection,
        "SELECT verification_recoveries.id, verification_recoveries.criterion_id,
                verification_recoveries.result_id,
                verification_recoveries.source_evidence_id,
                verification_recoveries.mandate_revision,
                verification_recoveries.action, verification_recoveries.status,
                verification_recoveries.requirement, verification_recoveries.created_at,
                verification_recoveries.resolved_at,
                verification_recoveries.mandate_revision = CASE commissions.status
                    WHEN 'verified_complete' THEN commissions.revision - 1
                    ELSE commissions.revision
                END
         FROM verification_recoveries
         JOIN commissions ON commissions.id = verification_recoveries.commission_id
         WHERE verification_recoveries.commission_id = ?1
         ORDER BY verification_recoveries.created_at, verification_recoveries.rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "criterion_id": row.get::<_, String>(1)?,
                "result_id": row.get::<_, String>(2)?,
                "source_evidence_id": row.get::<_, String>(3)?,
                "mandate_revision": row.get::<_, i64>(4)?,
                "action": row.get::<_, String>(5)?,
                "status": row.get::<_, String>(6)?,
                "requirement": row.get::<_, String>(7)?,
                "created_at": row.get::<_, i64>(8)?,
                "resolved_at": row.get::<_, Option<i64>>(9)?,
                "current": row.get::<_, bool>(10)?,
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
                attempts.status, attempts.started_at, attempts.completed_at,
                worker_leases.id, worker_leases.issued_at, worker_leases.expires_at,
                worker_leases.released_at, worker_leases.status
         FROM attempts
         JOIN assignments ON assignments.id = attempts.assignment_id
         LEFT JOIN worker_leases ON worker_leases.attempt_id = attempts.id
         WHERE assignments.commission_id = ?1 ORDER BY attempts.started_at, attempts.id",
        commission_id,
        |row| {
            let lease_id = row.get::<_, Option<String>>(6)?;
            let lease = match lease_id {
                Some(id) => Some(json!({
                    "id": id,
                    "issued_at": row.get::<_, i64>(7)?,
                    "expires_at": row.get::<_, i64>(8)?,
                    "released_at": row.get::<_, Option<i64>>(9)?,
                    "status": row.get::<_, String>(10)?,
                })),
                None => None,
            };
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "worker_configuration": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "started_at": row.get::<_, i64>(4)?,
                "completed_at": row.get::<_, Option<i64>>(5)?,
                "lease": lease,
            }))
        },
    )?;
    let results = query_values(
        connection,
        "SELECT results.id, results.attempt_id, results.output, results.artifact_revision,
                results.status, results.created_at, results.mandate_revision,
                results.base_revision, results.candidate_commits_json,
                results.changed_paths_json, results.artifacts_json,
                results.verification_outcomes_json, results.known_effects_json,
                results.integrated_artifact_revision
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
                "mandate_revision": row.get::<_, Option<i64>>(6)?,
                "base_revision": row.get::<_, Option<String>>(7)?,
                "candidate_commits": json_column(row, 8)?,
                "changed_paths": json_column(row, 9)?,
                "artifacts": json_column(row, 10)?,
                "verification_outcomes": json_column(row, 11)?,
                "known_effects": json_column(row, 12)?,
                "integrated_artifact_revision": row.get::<_, Option<String>>(13)?,
            }))
        },
    )?;
    let evidence = query_values(
        connection,
        "SELECT evidence.id, evidence.criterion_id, evidence.result_id,
                evidence.mandate_revision, evidence.artifact_revision,
                evidence.evidence_type, evidence.verifier_type, evidence.scope,
                evidence.verification_attempt_id, evidence.verifier_identity,
                evidence.verifier_configuration, evidence.verifier_kind,
                evidence.procedure_json, evidence.environment, evidence.outcome,
                evidence.observed, evidence.expected, evidence.material_contradiction,
                evidence.defect, evidence.producer_attempt_id, evidence.created_at,
                evidence.mandate_revision = CASE commissions.status
                    WHEN 'verified_complete' THEN commissions.revision - 1
                    ELSE commissions.revision
                END
                AND evidence.artifact_revision = commissions.artifact_revision
                AND evidence.scope IN ('integrated', 'external')
                AND results.status != 'superseded'
                AND evidence.evidence_type = criteria.required_evidence
                AND evidence.verifier_type = criteria.verifier_type
                AND evidence.verifier_configuration = criteria.verifier_configuration
                AND evidence.environment = criteria.verification_environment
                AND evidence.verifier_kind = criteria.verifier_kind
                AND evidence.expected = criteria.expected
         FROM evidence
         JOIN commissions ON commissions.id = evidence.commission_id
         JOIN criteria ON criteria.commission_id = evidence.commission_id
                      AND criteria.criterion_id = evidence.criterion_id
         JOIN results ON results.id = evidence.result_id
         WHERE evidence.commission_id = ?1
         ORDER BY evidence.created_at, evidence.rowid",
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

    let failed_criteria = criteria
        .iter()
        .filter(|criterion| criterion["status"] == "failed")
        .count();
    let unresolved_criteria = criteria
        .iter()
        .filter(|criterion| criterion["status"] != "passed")
        .count();
    let principal_pending = criteria.iter().any(|criterion| {
        criterion["status"] != "passed" && criterion["verifier_type"] == "principal"
    });
    let mut latest_current_evidence = HashMap::new();
    for record in evidence.iter().filter(|record| record["current"] == true) {
        if let Some(identity) = record["verifier_identity"].as_str() {
            latest_current_evidence.insert(
                (record["criterion_id"].clone(), identity.to_owned()),
                record,
            );
        }
    }
    let current_evidence = latest_current_evidence
        .values()
        .copied()
        .collect::<Vec<_>>();
    let material_contradiction = current_evidence
        .iter()
        .any(|record| record["material_contradiction"] == true);
    let required_gate_open = verification_gates
        .iter()
        .any(|gate| gate["current"] == true && gate["status"] == "open");
    let has_defect = |defect: &str| {
        current_evidence
            .iter()
            .any(|record| record["defect"] == defect)
    };
    let resource_blocked = assignments
        .iter()
        .any(|assignment| assignment["status"] == "resource_blocked");
    let verification = if material_contradiction {
        json!({
            "verdict": "uncertain",
            "next_action": "escalate",
            "reason": "Material contradictory Evidence remains unresolved.",
        })
    } else if unresolved_criteria > 0 && resource_blocked {
        json!({
            "verdict": "uncertain",
            "next_action": "block",
            "reason": "Criteria remain unresolved and no authorized Attempts remain.",
        })
    } else if required_gate_open {
        json!({
            "verdict": "uncertain",
            "next_action": "escalate",
            "reason": "A required Principal verification gate remains open.",
        })
    } else if has_defect("criterion") {
        json!({
            "verdict": "uncertain",
            "next_action": "escalate",
            "reason": "A criterion defect requires Principal clarification or amendment.",
        })
    } else if has_defect("verifier") {
        json!({
            "verdict": if failed_criteria > 0 { "failed" } else { "uncertain" },
            "next_action": "reroute",
            "reason": "The current verifier is a poor fit; use a distinct eligible verifier.",
        })
    } else if has_defect("result") {
        json!({
            "verdict": if failed_criteria > 0 { "failed" } else { "uncertain" },
            "next_action": "rework",
            "reason": "The current Result requires corrective work.",
        })
    } else if has_defect("environment") {
        json!({
            "verdict": "uncertain",
            "next_action": "retry",
            "reason": "The verification environment could not produce sufficient current Evidence.",
        })
    } else if failed_criteria > 0 {
        json!({
            "verdict": "failed",
            "next_action": "rework",
            "reason": format!("{failed_criteria} Acceptance Criterion failed."),
        })
    } else if unresolved_criteria > 0 {
        json!({
            "verdict": "uncertain",
            "next_action": if principal_pending { "escalate" } else { "retry" },
            "reason": format!("Current Evidence is insufficient for {unresolved_criteria} Acceptance Criterion."),
        })
    } else {
        json!({
            "verdict": "passed",
            "next_action": if commission["status"] == "verified_complete" { "closed" } else { "complete" },
            "reason": "Every current Acceptance Criterion passes.",
        })
    };

    Ok(json!({
        "commission": commission,
        "criteria": criteria,
        "criterion_versions": criterion_versions,
        "verification_gates": verification_gates,
        "verification_recoveries": verification_recoveries,
        "assignments": assignments,
        "attempts": attempts,
        "results": results,
        "evidence": evidence,
        "briefing": briefing,
        "events": events,
        "blockers": blockers,
        "attachments": attachments,
        "verification": verification,
    }))
}

fn json_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let encoded = row.get::<_, String>(index)?;
    serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
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
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "criterion_id": row.get::<_, String>(1)?,
        "result_id": row.get::<_, String>(2)?,
        "mandate_revision": row.get::<_, i64>(3)?,
        "artifact_revision": row.get::<_, String>(4)?,
        "evidence_type": row.get::<_, String>(5)?,
        "verifier_type": row.get::<_, String>(6)?,
        "scope": row.get::<_, String>(7)?,
        "verification_attempt_id": row.get::<_, String>(8)?,
        "verifier_identity": row.get::<_, String>(9)?,
        "verifier_configuration": row.get::<_, String>(10)?,
        "verifier_kind": row.get::<_, String>(11)?,
        "procedure": json_column(row, 12)?,
        "environment": row.get::<_, String>(13)?,
        "outcome": row.get::<_, String>(14)?,
        "observed": row.get::<_, String>(15)?,
        "expected": row.get::<_, String>(16)?,
        "material_contradiction": row.get::<_, bool>(17)?,
        "defect": row.get::<_, Option<String>>(18)?,
        "producer_attempt_id": row.get::<_, Option<String>>(19)?,
        "created_at": row.get::<_, i64>(20)?,
        "current": row.get::<_, bool>(21)?,
    }))
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
                      AND evidence.rowid = (
                          SELECT latest.rowid FROM evidence AS latest
                          WHERE latest.commission_id = criteria.commission_id
                            AND latest.criterion_id = criteria.criterion_id
                          ORDER BY latest.created_at DESC, latest.rowid DESC
                          LIMIT 1
                      )
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

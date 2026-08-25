use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::domain::AuthorityScopeType;
use crate::TyrionError;

use super::frontier::{Competition, OccupiedWork, Resources, Work};

pub(super) fn inspect_commission(
    connection: &Connection,
    commission_id: &str,
) -> Result<Value, TyrionError> {
    let commission = connection
        .query_row(
            "SELECT id, goal, status, revision, control_revision, accepted_at, completed_at,
                    artifact_revision, execution_json, plan_json, worker_requirements_json
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
                    "proposed_plan": row.get::<_, Option<String>>(9)?
                        .map(|encoded| serde_json::from_str::<Value>(&encoded))
                        .transpose()
                        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(
                            9,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        ))?,
                    "worker_requirements": json_column(row, 10)?,
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

    let plans = query_values(
        connection,
        "SELECT revision, source, reason, snapshot_json, created_at
         FROM commission_plans WHERE commission_id = ?1 ORDER BY revision",
        commission_id,
        |row| {
            Ok(json!({
                "revision": row.get::<_, i64>(0)?,
                "source": row.get::<_, String>(1)?,
                "reason": row.get::<_, String>(2)?,
                "snapshot": json_column(row, 3)?,
                "created_at": row.get::<_, i64>(4)?,
            }))
        },
    )?;
    let mut assignments = query_values(
        connection,
        "SELECT assignments.id, assignment_metadata.logical_id,
                assignments.plan_revision, assignments.status, assignments.created_at,
                assignment_metadata.purpose, assignment_metadata.read_scopes_json,
                assignment_metadata.write_scopes_json,
                assignment_metadata.concurrency_slots,
                assignment_metadata.max_storage_bytes,
                assignment_metadata.max_model_spend_cents,
                assignment_metadata.max_paid_service_spend_cents,
                assignment_metadata.competition_group,
                assignment_metadata.competition_uncertainty,
                assignment_metadata.competition_rule,
                assignment_metadata.position,
                assignment_routes.status, assignment_routes.selected_configuration_json,
                assignment_routes.rationale_json, assignment_routes.decided_at
         FROM assignments
         JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
         LEFT JOIN assignment_routes ON assignment_routes.assignment_id = assignments.id
         WHERE assignments.commission_id = ?1
         ORDER BY assignment_metadata.position, assignments.id",
        commission_id,
        |row| {
            let competition_group = row.get::<_, Option<String>>(12)?;
            let competition_uncertainty = row.get::<_, Option<String>>(13)?;
            let competition_rule = row.get::<_, Option<String>>(14)?;
            let competition = match (competition_group, competition_uncertainty, competition_rule) {
                (Some(group), Some(uncertainty), Some(comparison_rule)) => Some(json!({
                    "group": group,
                    "uncertainty": uncertainty,
                    "comparison_rule": comparison_rule,
                })),
                _ => None,
            };
            let route_status = row.get::<_, Option<String>>(16)?;
            let route = match route_status {
                Some(status) => {
                    let selected_configuration = row
                        .get::<_, Option<String>>(17)?
                        .map(|encoded| serde_json::from_str::<Value>(&encoded))
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                17,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    let rationale = json_column(row, 18)?;
                    Some(json!({
                        "status": status,
                        "selected_configuration": selected_configuration,
                        "rationale": rationale,
                        "decided_at": row.get::<_, i64>(19)?,
                    }))
                }
                None => None,
            };
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "logical_id": row.get::<_, String>(1)?,
                "plan_revision": row.get::<_, i64>(2)?,
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, i64>(4)?,
                "purpose": row.get::<_, String>(5)?,
                "read_scopes": json_column(row, 6)?,
                "write_scopes": json_column(row, 7)?,
                "resources": {
                    "concurrency_slots": row.get::<_, u32>(8)?,
                    "max_storage_bytes": row.get::<_, u64>(9)?,
                    "max_model_spend_cents": row.get::<_, u64>(10)?,
                    "max_paid_service_spend_cents": row.get::<_, u64>(11)?,
                },
                "competition": competition,
                "position": row.get::<_, i64>(15)?,
                "route": route,
            }))
        },
    )?;
    let assignment_skills = query_values(
        connection,
        "SELECT assignment_skill_defaults.assignment_id,
                assignment_skill_defaults.skill_name,
                assignment_skill_defaults.content_digest,
                assignment_skill_defaults.requirement,
                assignment_skill_defaults.provenance,
                assignment_skill_defaults.plan_revision,
                assignment_skill_defaults.delegation,
                assignment_skill_defaults.selected_at
         FROM assignment_skill_defaults
         JOIN assignments ON assignments.id = assignment_skill_defaults.assignment_id
         WHERE assignments.commission_id = ?1
         ORDER BY assignment_skill_defaults.assignment_id,
                  assignment_skill_defaults.skill_name",
        commission_id,
        |row| {
            Ok(json!({
                "assignment_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "content_digest": row.get::<_, String>(2)?,
                "requirement": row.get::<_, String>(3)?,
                "provenance": row.get::<_, String>(4)?,
                "plan_revision": row.get::<_, i64>(5)?,
                "delegation": row.get::<_, String>(6)?,
                "selected_at": row.get::<_, i64>(7)?,
            }))
        },
    )?;
    for assignment in &mut assignments {
        let assignment_id = assignment["id"].as_str();
        assignment["skill_defaults"] = Value::Array(
            assignment_skills
                .iter()
                .filter(|skill| skill["assignment_id"].as_str() == assignment_id)
                .map(|skill| {
                    let mut skill = skill.clone();
                    skill.as_object_mut().unwrap().remove("assignment_id");
                    skill
                })
                .collect(),
        );
    }
    let attempts = query_values(
        connection,
        "SELECT attempts.id, attempts.assignment_id, attempts.worker_configuration,
                attempts.status, attempts.started_at, attempts.completed_at,
                attempts.started_at_ms, attempts.execution_completed_at_ms, attempts.completed_at_ms,
                worker_leases.id, worker_leases.issued_at, worker_leases.expires_at,
                worker_leases.mandate_revision, worker_leases.released_at, worker_leases.status,
                resource_reservations.concurrency_slots,
                resource_reservations.storage_bytes,
                resource_reservations.model_spend_cents,
                resource_reservations.paid_service_spend_cents,
                resource_reservations.status, attempts.revision_disposition,
                EXISTS(
                    SELECT 1 FROM sandbox_cleanups
                    WHERE sandbox_cleanups.attempt_id = attempts.id
                )
         FROM attempts
         JOIN assignments ON assignments.id = attempts.assignment_id
         LEFT JOIN worker_leases ON worker_leases.attempt_id = attempts.id
         LEFT JOIN resource_reservations ON resource_reservations.attempt_id = attempts.id
         WHERE assignments.commission_id = ?1 ORDER BY attempts.started_at_ms, attempts.id",
        commission_id,
        |row| {
            let lease_id = row.get::<_, Option<String>>(9)?;
            let lease = match lease_id {
                Some(id) => Some(json!({
                    "id": id,
                    "issued_at": row.get::<_, i64>(10)?,
                    "expires_at": row.get::<_, i64>(11)?,
                    "mandate_revision": row.get::<_, i64>(12)?,
                    "released_at": row.get::<_, Option<i64>>(13)?,
                    "status": row.get::<_, String>(14)?,
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
                "started_at_ms": row.get::<_, i64>(6)?,
                "execution_completed_at_ms": row.get::<_, Option<i64>>(7)?,
                "completed_at_ms": row.get::<_, Option<i64>>(8)?,
                "lease": lease,
                "reservation": {
                    "concurrency_slots": row.get::<_, Option<u32>>(15)?,
                    "storage_bytes": row.get::<_, Option<u64>>(16)?,
                    "model_spend_cents": row.get::<_, Option<u64>>(17)?,
                    "paid_service_spend_cents": row.get::<_, Option<u64>>(18)?,
                    "status": row.get::<_, Option<String>>(19)?,
                },
                "revision_disposition": row.get::<_, String>(20)?,
                "cleanup_pending": row.get::<_, bool>(21)?,
            }))
        },
    )?;
    let now_ms = current_time_millis()?;
    let workers = query_values(
        connection,
        "SELECT workers.id, workers.handle, workers.status,
                workers.configuration_json, workers.routing_rationale_json,
                workers.native_session_id, workers.latest_activity,
                workers.activity_at_ms, workers.usage_json,
                attempts.started_at_ms, attempts.execution_completed_at_ms,
                assignments.id, assignment_metadata.logical_id, assignment_metadata.goal,
                attempts.id
         FROM workers
         JOIN attempts ON attempts.id = workers.attempt_id
         JOIN assignments ON assignments.id = workers.assignment_id
         JOIN assignment_metadata ON assignment_metadata.assignment_id = assignments.id
         WHERE workers.commission_id = ?1
         ORDER BY attempts.started_at_ms, workers.id",
        commission_id,
        |row| {
            let status = row.get::<_, String>(2)?;
            let started_at_ms = row.get::<_, i64>(9)?;
            let execution_completed_at_ms = row.get::<_, Option<i64>>(10)?;
            let elapsed_time_ms = execution_completed_at_ms
                .unwrap_or(now_ms)
                .saturating_sub(started_at_ms)
                .max(0);
            let available_controls = json!(["inspect"]);
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "handle": row.get::<_, String>(1)?,
                "status": status,
                "configuration": json_column(row, 3)?,
                "routing_rationale": json_column(row, 4)?,
                "native_session_id": row.get::<_, Option<String>>(5)?,
                "latest_meaningful_activity": row.get::<_, String>(6)?,
                "activity_at_ms": row.get::<_, i64>(7)?,
                "usage": json_column(row, 8)?,
                "started_at_ms": started_at_ms,
                "elapsed_time_ms": elapsed_time_ms,
                "assignment": {
                    "id": row.get::<_, String>(11)?,
                    "logical_id": row.get::<_, String>(12)?,
                    "goal": row.get::<_, String>(13)?,
                },
                "attempt_id": row.get::<_, String>(14)?,
                "available_controls": available_controls,
            }))
        },
    )?;
    let worker_commands = query_values(
        connection,
        "SELECT worker_commands.id, workers.handle, worker_commands.kind,
                worker_commands.payload_json, worker_commands.mandate_revision,
                worker_commands.status, worker_commands.created_at,
                worker_commands.attachment_id
         FROM worker_commands
         JOIN workers ON workers.id = worker_commands.worker_id
         WHERE worker_commands.commission_id = ?1
         ORDER BY worker_commands.created_at, worker_commands.rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "worker_handle": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "payload": json_column(row, 3)?,
                "mandate_revision": row.get::<_, i64>(4)?,
                "status": row.get::<_, String>(5)?,
                "created_at": row.get::<_, i64>(6)?,
                "attachment_id": row.get::<_, String>(7)?,
            }))
        },
    )?;
    let operation_requests = query_values(
        connection,
        "SELECT id, assignment_id, attempt_id, worker_lease_id, mandate_revision,
                plan_revision, operation, repository, target, parameters_json,
                destination, effect, consequences_json, limits_json,
                canonical_operation_json, operation_digest, classification, status,
                classification_reason, proposed_at, authorized_at, started_at,
                completed_at, receipt_json
         FROM operation_requests WHERE commission_id = ?1
         ORDER BY proposed_at, rowid",
        commission_id,
        |row| {
            let receipt = row
                .get::<_, Option<String>>(23)?
                .map(|encoded| serde_json::from_str::<Value>(&encoded))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        23,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "attempt_id": row.get::<_, String>(2)?,
                "worker_lease_id": row.get::<_, String>(3)?,
                "mandate_revision": row.get::<_, i64>(4)?,
                "plan_revision": row.get::<_, i64>(5)?,
                "operation": row.get::<_, String>(6)?,
                "repository": row.get::<_, Option<String>>(7)?,
                "target": row.get::<_, String>(8)?,
                "parameters": json_column(row, 9)?,
                "destination": row.get::<_, Option<String>>(10)?,
                "effect": row.get::<_, Option<String>>(11)?,
                "consequences": json_column(row, 12)?,
                "limits": json_column(row, 13)?,
                "canonical_operation": json_column(row, 14)?,
                "operation_digest": row.get::<_, String>(15)?,
                "classification": row.get::<_, String>(16)?,
                "status": row.get::<_, String>(17)?,
                "classification_reason": row.get::<_, String>(18)?,
                "proposed_at": row.get::<_, i64>(19)?,
                "authorized_at": row.get::<_, Option<i64>>(20)?,
                "started_at": row.get::<_, Option<i64>>(21)?,
                "completed_at": row.get::<_, Option<i64>>(22)?,
                "receipt": receipt,
            }))
        },
    )?;
    let approval_gates = query_values(
        connection,
        "SELECT approval_gates.id, approval_gates.operation_request_id,
                approval_gates.operation_digest, approval_gates.status,
                approval_gates.opened_at, approval_gates.authorized_at,
                approval_gates.consumed_at, approval_gates.invalidated_at,
                operation_requests.canonical_operation_json,
                operation_requests.target, operation_requests.mandate_revision,
                operation_requests.plan_revision, operation_requests.consequences_json,
                operation_requests.limits_json
         FROM approval_gates
         JOIN operation_requests ON operation_requests.id = approval_gates.operation_request_id
         WHERE approval_gates.commission_id = ?1
         ORDER BY approval_gates.opened_at, approval_gates.rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "operation_request_id": row.get::<_, String>(1)?,
                "operation_digest": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "opened_at": row.get::<_, i64>(4)?,
                "authorized_at": row.get::<_, Option<i64>>(5)?,
                "consumed_at": row.get::<_, Option<i64>>(6)?,
                "invalidated_at": row.get::<_, Option<i64>>(7)?,
                "canonical_operation": json_column(row, 8)?,
                "exact_target": row.get::<_, String>(9)?,
                "governing_revision": {
                    "mandate": row.get::<_, i64>(10)?,
                    "plan": row.get::<_, i64>(11)?,
                },
                "consequences": json_column(row, 12)?,
                "limits": json_column(row, 13)?,
                "confirmation_path": "principal_control",
            }))
        },
    )?;
    let credential_grants = query_values(
        connection,
        "SELECT id, assignment_id, attempt_id, worker_lease_id, mandate_revision,
                plan_revision, capability, destination, exposure,
                credential_expires_at, revocation, status, created_at, consumed_at, revoked_at
         FROM credential_grants WHERE commission_id = ?1
         ORDER BY created_at, rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "attempt_id": row.get::<_, String>(2)?,
                "worker_lease_id": row.get::<_, String>(3)?,
                "mandate_revision": row.get::<_, i64>(4)?,
                "plan_revision": row.get::<_, i64>(5)?,
                "capability": row.get::<_, String>(6)?,
                "destination": row.get::<_, String>(7)?,
                "exposure": row.get::<_, String>(8)?,
                "credential_expires_at": row.get::<_, i64>(9)?,
                "revocation": row.get::<_, String>(10)?,
                "status": row.get::<_, String>(11)?,
                "created_at": row.get::<_, i64>(12)?,
                "consumed_at": row.get::<_, Option<i64>>(13)?,
                "revoked_at": row.get::<_, Option<i64>>(14)?,
                "credential_reference": "redacted",
            }))
        },
    )?;
    let credential_exposure_grants = query_values(
        connection,
        "SELECT credential_exposure_grants.id,
                credential_exposure_grants.credential_grant_id,
                credential_exposure_grants.operation_request_id,
                credential_exposure_grants.operation_digest,
                credential_exposure_grants.status,
                credential_exposure_grants.authorized_at,
                credential_exposure_grants.consumed_at,
                credential_exposure_grants.revoked_at
         FROM credential_exposure_grants
         JOIN credential_grants
           ON credential_grants.id = credential_exposure_grants.credential_grant_id
         WHERE credential_grants.commission_id = ?1
         ORDER BY credential_exposure_grants.authorized_at, credential_exposure_grants.rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "credential_grant_id": row.get::<_, String>(1)?,
                "operation_request_id": row.get::<_, String>(2)?,
                "operation_digest": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
                "authorized_at": row.get::<_, i64>(5)?,
                "consumed_at": row.get::<_, Option<i64>>(6)?,
                "revoked_at": row.get::<_, Option<i64>>(7)?,
            }))
        },
    )?;
    let commission_amendments = query_values(
        connection,
        "SELECT id, base_revision, authority_json, resource_ceilings_json, reason,
                diff_json, amendment_digest, impact_json, revalidation_json, status,
                proposed_at, accepted_at
         FROM commission_amendments WHERE commission_id = ?1
         ORDER BY proposed_at, rowid",
        commission_id,
        |row| {
            let revalidation = row
                .get::<_, Option<String>>(8)?
                .map(|encoded| serde_json::from_str::<Value>(&encoded))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "base_revision": row.get::<_, i64>(1)?,
                "authority": json_column(row, 2)?,
                "resource_ceilings": json_column(row, 3)?,
                "reason": row.get::<_, String>(4)?,
                "diff": json_column(row, 5)?,
                "amendment_digest": row.get::<_, String>(6)?,
                "impact": json_column(row, 7)?,
                "revalidation": revalidation,
                "status": row.get::<_, String>(9)?,
                "proposed_at": row.get::<_, i64>(10)?,
                "accepted_at": row.get::<_, Option<i64>>(11)?,
            }))
        },
    )?;
    let mut results = query_values(
        connection,
        "SELECT results.id, results.attempt_id, results.output, results.artifact_revision,
                results.status, results.created_at, results.mandate_revision,
                results.plan_revision, results.base_revision, results.candidate_commits_json,
                results.changed_paths_json, results.artifacts_json,
                results.verification_outcomes_json, results.known_effects_json,
                results.integrated_artifact_revision, results.revision_disposition
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
                "plan_revision": row.get::<_, Option<i64>>(7)?,
                "base_revision": row.get::<_, Option<String>>(8)?,
                "candidate_commits": json_column(row, 9)?,
                "changed_paths": json_column(row, 10)?,
                "artifacts": json_column(row, 11)?,
                "verification_outcomes": json_column(row, 12)?,
                "known_effects": json_column(row, 13)?,
                "integrated_artifact_revision": row.get::<_, Option<String>>(14)?,
                "revision_disposition": row.get::<_, String>(15)?,
            }))
        },
    )?;
    let result_skill_executions = query_values(
        connection,
        "SELECT result_skill_executions.result_id,
                result_skill_executions.skill_name,
                result_skill_executions.content_digest,
                result_skill_executions.requirement,
                result_skill_executions.provenance,
                result_skill_executions.worker_configuration,
                result_skill_executions.assignment_class,
                result_skill_executions.verification_outcome,
                result_skill_executions.corrections,
                result_skill_executions.cost_cents,
                result_skill_executions.latency_ms,
                result_skill_executions.principal_intervention,
                result_skill_executions.delegation
         FROM result_skill_executions
         JOIN results ON results.id = result_skill_executions.result_id
         JOIN attempts ON attempts.id = results.attempt_id
         JOIN assignments ON assignments.id = attempts.assignment_id
         WHERE assignments.commission_id = ?1
         ORDER BY result_skill_executions.result_id,
                  result_skill_executions.skill_name",
        commission_id,
        |row| {
            Ok(json!({
                "result_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "content_digest": row.get::<_, String>(2)?,
                "requirement": row.get::<_, String>(3)?,
                "provenance": row.get::<_, String>(4)?,
                "worker_configuration": row.get::<_, String>(5)?,
                "assignment_class": row.get::<_, String>(6)?,
                "verification_outcome": row.get::<_, String>(7)?,
                "corrections": row.get::<_, u64>(8)?,
                "cost_cents": row.get::<_, u64>(9)?,
                "latency_ms": row.get::<_, u64>(10)?,
                "principal_intervention": row.get::<_, bool>(11)?,
                "delegation": row.get::<_, String>(12)?,
            }))
        },
    )?;
    for result in &mut results {
        let result_id = result["id"].as_str();
        result["skill_executions"] = Value::Array(
            result_skill_executions
                .iter()
                .filter(|skill| skill["result_id"].as_str() == result_id)
                .map(|skill| {
                    let mut skill = skill.clone();
                    skill.as_object_mut().unwrap().remove("result_id");
                    skill
                })
                .collect(),
        );
    }
    let skill_associations = query_values(
        connection,
        "SELECT id, assignment_id, attempt_id, result_id, skill_name, content_digest,
                worker_configuration, harness, assignment_class, observation,
                verification_outcome, corrections, cost_cents, latency_ms,
                principal_intervention, evidence_ids_json, scope_json,
                confidence_basis_points, observed_at
         FROM skill_associations WHERE commission_id = ?1
         ORDER BY observed_at, id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "attempt_id": row.get::<_, String>(2)?,
                "result_id": row.get::<_, Option<String>>(3)?,
                "skill_version": {
                    "name": row.get::<_, String>(4)?,
                    "content_digest": row.get::<_, String>(5)?,
                },
                "worker_configuration": row.get::<_, String>(6)?,
                "harness": row.get::<_, String>(7)?,
                "assignment_class": row.get::<_, String>(8)?,
                "observation": row.get::<_, String>(9)?,
                "verification_outcome": row.get::<_, String>(10)?,
                "corrections": row.get::<_, u64>(11)?,
                "cost_cents": row.get::<_, u64>(12)?,
                "latency_ms": row.get::<_, u64>(13)?,
                "principal_intervention": row.get::<_, bool>(14)?,
                "evidence": json_column(row, 15)?,
                "scope": json_column(row, 16)?,
                "confidence_basis_points": row.get::<_, u64>(17)?,
                "observed_at": row.get::<_, i64>(18)?,
                "causal": false,
                "global_ban": false,
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
    let attention_conditions = query_values(
        connection,
        "SELECT id, assignment_id, code, requirement, status, created_at, resolved_at
         FROM attention_conditions
         WHERE commission_id = ?1
         ORDER BY created_at, id",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "code": row.get::<_, String>(2)?,
                "requirement": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
                "created_at": row.get::<_, i64>(5)?,
                "resolved_at": row.get::<_, Option<i64>>(6)?,
            }))
        },
    )?;
    let recovery_history = query_values(
        connection,
        "SELECT id, assignment_id, attempt_id, cause, classification,
                equivalence_key, action, requirement, created_at
         FROM attempt_recoveries WHERE commission_id = ?1 ORDER BY created_at, rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "assignment_id": row.get::<_, String>(1)?,
                "attempt_id": row.get::<_, String>(2)?,
                "cause": row.get::<_, String>(3)?,
                "classification": row.get::<_, String>(4)?,
                "equivalence_key": row.get::<_, String>(5)?,
                "action": row.get::<_, String>(6)?,
                "requirement": row.get::<_, String>(7)?,
                "created_at": row.get::<_, i64>(8)?,
            }))
        },
    )?;
    let restart_recoveries = query_values(
        connection,
        "SELECT attempt_id, decision, process_identity, native_session_identity,
                acknowledged_state, lease_validity, current_authority, containment,
                cleanup_confirmed, requirement, created_at
         FROM restart_recoveries WHERE commission_id = ?1 ORDER BY created_at, rowid",
        commission_id,
        |row| {
            Ok(json!({
                "attempt_id": row.get::<_, String>(0)?,
                "decision": row.get::<_, String>(1)?,
                "proofs": {
                    "process_identity": row.get::<_, bool>(2)?,
                    "native_session_identity": row.get::<_, bool>(3)?,
                    "acknowledged_state": row.get::<_, bool>(4)?,
                    "lease_validity": row.get::<_, bool>(5)?,
                    "current_authority": row.get::<_, bool>(6)?,
                    "containment": row.get::<_, bool>(7)?,
                },
                "cleanup_confirmed": row.get::<_, bool>(8)?,
                "requirement": row.get::<_, String>(9)?,
                "created_at": row.get::<_, i64>(10)?,
            }))
        },
    )?;
    let watchdog_findings = query_values(
        connection,
        "SELECT id, assignment_id, attempt_id, signal, action, details, created_at
         FROM watchdog_findings WHERE commission_id = ?1 ORDER BY created_at, rowid",
        commission_id,
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "scope": {
                    "assignment_id": row.get::<_, String>(1)?,
                    "attempt_id": row.get::<_, String>(2)?,
                },
                "signal": row.get::<_, String>(3)?,
                "action": row.get::<_, String>(4)?,
                "details": row.get::<_, String>(5)?,
                "created_at": row.get::<_, i64>(6)?,
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

    let concurrency_evidence = events
        .iter()
        .filter(|event| event["type"] == "useful_concurrency_observed")
        .next_back()
        .map(|event| &event["payload"]);
    let metric = |name: &str| {
        concurrency_evidence
            .and_then(|payload| payload[name].as_u64())
            .unwrap_or(0)
    };
    let overlap_millis = metric("overlap_millis");
    let serial_execution_millis = metric("serial_execution_millis");
    let activity_journal = json!({
        "useful_concurrency": {
            "occurred": overlap_millis > 0,
            "overlap_millis": overlap_millis,
            "elapsed_time_reduction_millis": metric("elapsed_time_reduction_millis"),
            "serial_execution_millis": serial_execution_millis,
            "serial_attempt_millis": serial_execution_millis,
            "parallel_execution_window_millis": metric("parallel_execution_window_millis"),
            "end_to_end_elapsed_millis": metric("end_to_end_elapsed_millis"),
            "success_metric": "verified execution elapsed-time reduction",
        }
    });
    let occupied = attempts
        .iter()
        .filter(|attempt| attempt["status"] == "running")
        .filter_map(|attempt| {
            let assignment = assignments
                .iter()
                .find(|assignment| assignment["id"] == attempt["assignment_id"])?;
            Some(OccupiedWork {
                write_scopes: json_string_vec(&assignment["write_scopes"]),
                competition: json_competition(&assignment["competition"]),
            })
        })
        .collect::<Vec<_>>();
    let reserved_concurrency = attempts
        .iter()
        .filter(|attempt| attempt["reservation"]["status"] == "active")
        .filter_map(|attempt| attempt["reservation"]["concurrency_slots"].as_u64())
        .sum::<u64>();
    let reserved_storage = attempts
        .iter()
        .filter(|attempt| attempt["reservation"]["status"] == "active")
        .filter_map(|attempt| attempt["reservation"]["storage_bytes"].as_u64())
        .sum::<u64>();
    let reserved_model_spend = attempts
        .iter()
        .filter_map(|attempt| attempt["reservation"]["model_spend_cents"].as_u64())
        .sum::<u64>();
    let reserved_paid_spend = attempts
        .iter()
        .filter_map(|attempt| attempt["reservation"]["paid_service_spend_cents"].as_u64())
        .sum::<u64>();
    let candidates = assignments
        .iter()
        .filter(|assignment| assignment["status"] == "ready")
        .filter(|assignment| assignment["route"]["status"] != "attention_required")
        .filter(|assignment| {
            !attempts.iter().any(|attempt| {
                attempt["assignment_id"] == assignment["id"] && attempt["cleanup_pending"] == true
            })
        })
        .map(|assignment| {
            let resources = &assignment["resources"];
            Work {
                item: assignment,
                write_scopes: json_string_vec(&assignment["write_scopes"]),
                competition: json_competition(&assignment["competition"]),
                resources: Resources {
                    concurrency: resources["concurrency_slots"].as_u64().unwrap_or(u64::MAX),
                    storage: resources["max_storage_bytes"].as_u64().unwrap_or(u64::MAX),
                    model_spend: resources["max_model_spend_cents"]
                        .as_u64()
                        .unwrap_or(u64::MAX),
                    paid_spend: resources["max_paid_service_spend_cents"]
                        .as_u64()
                        .unwrap_or(u64::MAX),
                },
            }
        })
        .collect();
    let frontier = super::frontier::select(
        candidates,
        occupied,
        Resources {
            concurrency: reserved_concurrency,
            storage: reserved_storage,
            model_spend: reserved_model_spend,
            paid_spend: reserved_paid_spend,
        },
        Resources {
            concurrency: commission["resource_ceilings"]["max_worker_concurrency"]
                .as_u64()
                .unwrap_or(0),
            storage: commission["resource_ceilings"]["max_storage_bytes"]
                .as_u64()
                .unwrap_or(0),
            model_spend: commission["resource_ceilings"]["max_model_spend_cents"]
                .as_u64()
                .unwrap_or(0),
            paid_spend: commission["resource_ceilings"]["max_paid_service_spend_cents"]
                .as_u64()
                .unwrap_or(0),
        },
    );
    let execution_frontier = frontier
        .selected
        .into_iter()
        .map(|assignment| {
            json!({
                "assignment_id": assignment["id"],
                "logical_id": assignment["logical_id"],
                "purpose": assignment["purpose"],
                "plan_revision": assignment["plan_revision"],
            })
        })
        .collect::<Vec<_>>();
    let frontier_holds = frontier
        .held
        .into_iter()
        .map(|(assignment, reason)| {
            json!({
                "assignment_id": assignment["id"],
                "logical_id": assignment["logical_id"],
                "reason": reason.as_str(),
            })
        })
        .collect::<Vec<_>>();

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

    let passed_criteria = criteria
        .iter()
        .filter(|criterion| criterion["status"] == "passed")
        .filter_map(|criterion| criterion["id"].as_str())
        .collect::<Vec<_>>();
    let unresolved_criterion_ids = criteria
        .iter()
        .filter(|criterion| criterion["status"] != "passed")
        .filter_map(|criterion| criterion["id"].as_str())
        .collect::<Vec<_>>();
    let running_attempt = attempts
        .iter()
        .any(|attempt| attempt["status"] == "running");
    let effect_cleanup_blocked = operation_requests.iter().any(|operation| {
        operation["status"] == "uncertain" && operation["receipt"]["containment_confirmed"] == false
    });
    let exact_next_requirement = blockers
        .last()
        .and_then(|blocker| blocker["requirement"].as_str())
        .or_else(|| {
            attention_conditions
                .iter()
                .rev()
                .find(|condition| condition["status"] == "open")
                .and_then(|condition| condition["requirement"].as_str())
        })
        .or_else(|| {
            restart_recoveries
                .iter()
                .rev()
                .find(|recovery| recovery["cleanup_confirmed"] == false)
                .and_then(|recovery| recovery["requirement"].as_str())
        })
        .or_else(|| {
            operation_requests
                .iter()
                .rev()
                .find(|operation| {
                    operation["status"] == "uncertain"
                        && operation["receipt"]["containment_confirmed"] == false
                })
                .and_then(|operation| operation["receipt"]["requirement"].as_str())
        })
        .unwrap_or_else(|| {
            verification["reason"]
                .as_str()
                .unwrap_or("Provide sufficient current Evidence to resolve the remaining criteria.")
        });
    let no_useful_frontier = execution_frontier.is_empty() && !running_attempt;
    let recovery_state = match commission["status"].as_str() {
        Some("verified_complete") => "recovered",
        Some("paused") if effect_cleanup_blocked => "blocked",
        Some("paused") => "paused",
        Some("cancelled") => "cancelled",
        _ if unresolved_criteria > 0 && no_useful_frontier => "blocked",
        _ => "running",
    };
    let retained_artifacts = results
        .iter()
        .map(|result| {
            json!({
                "result_id": result["id"],
                "artifact_revision": result["artifact_revision"],
                "integrated_artifact_revision": result["integrated_artifact_revision"],
                "artifacts": result["artifacts"],
                "disposition": result["revision_disposition"],
            })
        })
        .collect::<Vec<_>>();
    let resumable_blocker = (recovery_state == "blocked").then(|| {
        json!({
            "passed_criteria": passed_criteria,
            "unresolved_criteria": unresolved_criterion_ids,
            "retained_artifacts": retained_artifacts,
            "evidence": evidence,
            "failed_approaches": recovery_history,
            "resource_use": {
                "attempts": attempts.len(),
                "reserved_model_spend_cents": reserved_model_spend,
                "reserved_paid_service_spend_cents": reserved_paid_spend,
                "active_concurrency_slots": reserved_concurrency,
                "active_storage_bytes": reserved_storage,
            },
            "exact_next_requirement": exact_next_requirement,
        })
    });
    let irreversible_effects = operation_requests
        .iter()
        .filter(|operation| operation["status"] == "confirmed")
        .map(|operation| {
            json!({
                "operation_request_id": operation["id"],
                "operation_digest": operation["operation_digest"],
                "receipt": operation["receipt"],
            })
        })
        .collect::<Vec<_>>();
    let revoked_operation_request_ids = operation_requests
        .iter()
        .filter(|operation| operation["status"] == "revoked")
        .map(|operation| operation["id"].clone())
        .collect::<Vec<_>>();
    let uncertain_operation_request_ids = operation_requests
        .iter()
        .filter(|operation| operation["status"] == "uncertain")
        .map(|operation| operation["id"].clone())
        .collect::<Vec<_>>();
    let recovery = json!({
        "state": recovery_state,
        "resumable": recovery_state == "paused" || recovery_state == "blocked",
        "resumable_blocker": resumable_blocker,
        "cancellation": (recovery_state == "cancelled").then(|| json!({
            "authority_grants_revoked": true,
            "rollback_claimed": false,
            "integrated_artifact_revision": commission["artifact_revision"],
            "retained_results": results.len(),
            "retained_evidence": evidence.len(),
            "irreversible_effects": irreversible_effects,
            "revoked_operation_request_ids": revoked_operation_request_ids,
            "affected_in_flight_operation_ids": uncertain_operation_request_ids,
        })),
    });
    let watchdog = json!({
        "monitored_signals": [
            "stall",
            "unhealthy_retry_pattern",
            "repeated_verification_failure",
            "abnormal_resource_use",
            "lost_liveness",
            "invalid_authority",
        ],
        "findings": watchdog_findings,
    });

    Ok(json!({
        "commission": commission,
        "criteria": criteria,
        "criterion_versions": criterion_versions,
        "verification_gates": verification_gates,
        "verification_recoveries": verification_recoveries,
        "plans": plans,
        "execution_frontier": execution_frontier,
        "frontier_holds": frontier_holds,
        "assignments": assignments,
        "attempts": attempts,
        "workers": workers,
        "worker_commands": worker_commands,
        "operation_requests": operation_requests,
        "approval_gates": approval_gates,
        "credential_grants": credential_grants,
        "credential_exposure_grants": credential_exposure_grants,
        "commission_amendments": commission_amendments,
        "results": results,
        "skill_associations": skill_associations,
        "evidence": evidence,
        "briefing": briefing,
        "events": events,
        "blockers": blockers,
        "attention_conditions": attention_conditions,
        "recovery_history": recovery_history,
        "restart_recoveries": restart_recoveries,
        "recovery": recovery,
        "watchdog": watchdog,
        "attachments": attachments,
        "activity_journal": activity_journal,
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

fn json_string_vec(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn json_competition(value: &Value) -> Option<Competition> {
    Some(Competition {
        group: value["group"].as_str()?.to_owned(),
        uncertainty: value["uncertainty"].as_str()?.to_owned(),
        rule: value["comparison_rule"].as_str()?.to_owned(),
    })
}

fn current_time_millis() -> Result<i64, TyrionError> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| TyrionError::InvalidRequest("system clock is before Unix epoch".into()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| TyrionError::InvalidRequest("system clock does not fit in SQLite".into()))
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

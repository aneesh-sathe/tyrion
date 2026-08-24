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
    let assignments = query_values(
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
    let attempts = query_values(
        connection,
        "SELECT attempts.id, attempts.assignment_id, attempts.worker_configuration,
                attempts.status, attempts.started_at, attempts.completed_at,
                attempts.started_at_ms, attempts.execution_completed_at_ms, attempts.completed_at_ms,
                worker_leases.id, worker_leases.issued_at, worker_leases.expires_at,
                worker_leases.released_at, worker_leases.status,
                resource_reservations.concurrency_slots,
                resource_reservations.storage_bytes,
                resource_reservations.model_spend_cents,
                resource_reservations.paid_service_spend_cents,
                resource_reservations.status
         FROM attempts
         JOIN assignments ON assignments.id = attempts.assignment_id
         LEFT JOIN worker_leases ON worker_leases.attempt_id = attempts.id
         LEFT JOIN resource_reservations ON resource_reservations.attempt_id = attempts.id
         WHERE assignments.commission_id = ?1 ORDER BY attempts.started_at, attempts.id",
        commission_id,
        |row| {
            let lease_id = row.get::<_, Option<String>>(9)?;
            let lease = match lease_id {
                Some(id) => Some(json!({
                    "id": id,
                    "issued_at": row.get::<_, i64>(10)?,
                    "expires_at": row.get::<_, i64>(11)?,
                    "released_at": row.get::<_, Option<i64>>(12)?,
                    "status": row.get::<_, String>(13)?,
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
                    "concurrency_slots": row.get::<_, Option<u32>>(14)?,
                    "storage_bytes": row.get::<_, Option<u64>>(15)?,
                    "model_spend_cents": row.get::<_, Option<u64>>(16)?,
                    "paid_service_spend_cents": row.get::<_, Option<u64>>(17)?,
                    "status": row.get::<_, Option<String>>(18)?,
                },
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
    let results = query_values(
        connection,
        "SELECT results.id, results.attempt_id, results.output, results.artifact_revision,
                results.status, results.created_at, results.mandate_revision,
                results.plan_revision, results.base_revision, results.candidate_commits_json,
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
                "plan_revision": row.get::<_, Option<i64>>(7)?,
                "base_revision": row.get::<_, Option<String>>(8)?,
                "candidate_commits": json_column(row, 9)?,
                "changed_paths": json_column(row, 10)?,
                "artifacts": json_column(row, 11)?,
                "verification_outcomes": json_column(row, 12)?,
                "known_effects": json_column(row, 13)?,
                "integrated_artifact_revision": row.get::<_, Option<String>>(14)?,
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
        "results": results,
        "evidence": evidence,
        "briefing": briefing,
        "events": events,
        "blockers": blockers,
        "attention_conditions": attention_conditions,
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

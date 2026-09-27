//! A Commission at a glance: what is running, what is waiting, what is
//! blocked, what is done, and what needs the Principal. The full projection
//! is the record; at ten Workers it is half a megabyte, which drowns a host
//! model's context in Evidence and plan history it did not ask for.
//!
//! The digest is derived from the full projection, so it can never disagree
//! with it, and the full projection stays one request away.

use std::collections::HashMap;

use serde_json::{json, Value};

const ACTIVITY_CHARS: usize = 120;
const DETAIL_CHARS: usize = 400;

pub(crate) fn commission_digest(projection: &Value) -> Value {
    let assignments = array(&projection["assignments"]);
    let logical_ids: HashMap<&str, &str> = assignments
        .iter()
        .filter_map(|assignment| {
            Some((
                assignment["id"].as_str()?,
                assignment["logical_id"].as_str()?,
            ))
        })
        .collect();
    let logical = |assignment_id: &Value| {
        assignment_id
            .as_str()
            .and_then(|id| logical_ids.get(id).copied())
            .unwrap_or("commission")
            .to_owned()
    };

    let needs_you = needs_principal(projection, &logical);
    let holds: HashMap<&str, &Value> = array(&projection["frontier_holds"])
        .iter()
        .filter_map(|hold| Some((hold["assignment_id"].as_str()?, hold)))
        .collect();
    let blockers: HashMap<&str, &str> = array(&projection["blockers"])
        .iter()
        .filter_map(|blocker| {
            Some((
                blocker["assignment_id"].as_str()?,
                blocker["requirement"].as_str()?,
            ))
        })
        .collect();

    let mut counts: HashMap<&'static str, u64> = HashMap::new();
    // Every Worker's reported cost, including replaced Attempts: money spent on
    // work that was superseded is still money spent.
    let total_cost_cents: u64 = array(&projection["workers"])
        .iter()
        .filter_map(|worker| worker["usage"]["cost_cents"].as_u64())
        .sum();
    let total_tokens: u64 = array(&projection["workers"]).iter().map(tokens).sum();
    let mut rows = Vec::new();
    for assignment in assignments {
        let id = assignment["id"].as_str().unwrap_or_default();
        let state = state_of(assignment, holds.contains_key(id));
        *counts.entry(state).or_default() += 1;
        if matches!(state, "replaced" | "cancelled") {
            continue;
        }
        let workers: Vec<&Value> = array(&projection["workers"])
            .iter()
            .filter(|worker| worker["assignment"]["id"] == assignment["id"])
            .collect();
        let cost_cents: u64 = workers
            .iter()
            .filter_map(|worker| worker["usage"]["cost_cents"].as_u64())
            .sum();
        let mut row = json!({
            "id": assignment["logical_id"],
            "state": state,
            "attempts": workers.len(),
            "cost_cents": cost_cents,
            "tokens": workers.iter().copied().map(tokens).sum::<u64>(),
        });
        if let Some(worker) = workers.last() {
            let configuration = &worker["configuration"];
            row["worker"] = json!(format!(
                "{} ({} {})",
                worker["handle"].as_str().unwrap_or("?"),
                configuration["harness"].as_str().unwrap_or("?"),
                configuration["model"].as_str().unwrap_or("?"),
            ));
            row["elapsed_s"] = json!(worker["elapsed_time_ms"].as_u64().unwrap_or(0) / 1000);
            if state == "running" {
                if let Some(activity) = worker["latest_meaningful_activity"].as_str() {
                    row["activity"] = json!(clip(activity, ACTIVITY_CHARS));
                }
            }
        }
        let detail = match state {
            "blocked" => blockers
                .get(id)
                .map(|requirement| clip(requirement, DETAIL_CHARS)),
            "held" => holds.get(id).map(|hold| {
                hold["detail"]
                    .as_str()
                    .map(|detail| clip(detail, DETAIL_CHARS))
                    .unwrap_or_else(|| hold["reason"].as_str().unwrap_or_default().to_owned())
            }),
            _ => None,
        };
        if let Some(detail) = detail {
            row["detail"] = json!(detail);
        }
        rows.push(row);
    }

    let count = |state: &str| counts.get(state).copied().unwrap_or(0);
    let total = rows.len() as u64;
    let mut headline = format!(
        "{} of {total} done, {} running, {} queued, {} held, {} blocked; {total_cost_cents}¢ reported",
        count("done"),
        count("running"),
        count("queued"),
        count("held"),
        count("blocked"),
    );
    if !needs_you.is_empty() {
        headline = format!("NEEDS YOU ({}): {headline}", needs_you.len());
    }
    let commission = &projection["commission"];
    let mut digest = json!({
        "headline": headline,
        "needs_you": needs_you,
        "commission": {
            "id": commission["id"],
            "status": commission["status"],
            "goal": clip(commission["goal"].as_str().unwrap_or_default(), DETAIL_CHARS),
        },
        "verification": projection["verification"],
        "assignments": rows,
        "cost": {
            "reported_cents": total_cost_cents,
            "tokens": total_tokens,
            "note": "Observed, not bounded: the provider's spend cap is the control. A subscription reports 0 cents, so tokens are the usage signal.",
        },
        "detail": "Call tyrion_status with {\"detail\": true} for the full record.",
    });
    if count("replaced") + count("cancelled") > 0 {
        digest["replaced_or_cancelled"] = json!(count("replaced") + count("cancelled"));
    }
    if !projection["review"].is_null() {
        digest["review"] = projection["review"].clone();
    }
    let concurrency = &projection["activity_journal"]["useful_concurrency"];
    if concurrency["occurred"] == true {
        digest["parallel_speedup"] = json!({
            "serial_s": concurrency["serial_execution_millis"].as_u64().unwrap_or(0) / 1000,
            "parallel_window_s": concurrency["parallel_execution_window_millis"].as_u64().unwrap_or(0) / 1000,
            "saved_s": concurrency["elapsed_time_reduction_millis"].as_u64().unwrap_or(0) / 1000,
        });
    }
    digest
}

/// One word per Assignment, so a factory of twenty reads at a glance.
fn state_of(assignment: &Value, held: bool) -> &'static str {
    match assignment["status"].as_str().unwrap_or_default() {
        "running" | "verification_pending" => "running",
        "ready" if held => "held",
        "ready" => "queued",
        "accepted" => "done",
        "resource_blocked" | "verification_failed" => "blocked",
        "attention_required" => "needs_you",
        "superseded" => "replaced",
        "cancelled" => "cancelled",
        _ => "other",
    }
}

/// Everything waiting on the Principal, first in the digest so it cannot be
/// missed: open Approval Gates, routing or recovery Attention Conditions,
/// open Principal verification gates, proposed Amendments, and Blockers.
fn needs_principal(projection: &Value, logical: &impl Fn(&Value) -> String) -> Vec<Value> {
    let mut needs = Vec::new();
    for gate in array(&projection["approval_gates"]) {
        if gate["status"] == "open" {
            needs.push(json!({
                "kind": "approval_gate",
                "id": gate["id"],
                "what": clip(
                    &gate["canonical_operation"].to_string(),
                    DETAIL_CHARS,
                ),
            }));
        }
    }
    for condition in array(&projection["attention_conditions"]) {
        if condition["status"] == "open" {
            needs.push(json!({
                "kind": "attention",
                "assignment": logical(&condition["assignment_id"]),
                "what": clip(condition["requirement"].as_str().unwrap_or_default(), DETAIL_CHARS),
            }));
        }
    }
    for gate in array(&projection["verification_gates"]) {
        if gate["status"] == "open" && gate["current"] == true {
            needs.push(json!({
                "kind": "verification",
                "criterion": gate["criterion_id"],
                "what": "Record your verdict on this criterion.",
            }));
        }
    }
    for amendment in array(&projection["commission_amendments"]) {
        if amendment["status"] == "proposed" {
            needs.push(json!({
                "kind": "amendment",
                "id": amendment["id"],
                "what": clip(amendment["reason"].as_str().unwrap_or_default(), DETAIL_CHARS),
            }));
        }
    }
    let settled = |assignment_id: &Value| {
        array(&projection["assignments"]).iter().any(|assignment| {
            assignment["id"] == *assignment_id
                && matches!(
                    assignment["status"].as_str(),
                    Some("accepted" | "superseded" | "cancelled")
                )
        })
    };
    for blocker in array(&projection["blockers"]) {
        if !settled(&blocker["assignment_id"]) {
            needs.push(json!({
                "kind": "blocker",
                "assignment": logical(&blocker["assignment_id"]),
                "what": clip(blocker["requirement"].as_str().unwrap_or_default(), DETAIL_CHARS),
            }));
        }
    }
    needs
}

fn tokens(worker: &Value) -> u64 {
    let usage = &worker["usage"];
    usage["input_tokens"].as_u64().unwrap_or(0) + usage["output_tokens"].as_u64().unwrap_or(0)
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn clip(text: &str, limit: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= limit {
        return single_line;
    }
    let mut clipped: String = single_line.chars().take(limit - 1).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twenty Assignments with everything a factory run produces, including
    /// the bulky Evidence and plan history the digest must leave out.
    fn factory_projection() -> Value {
        let states = [
            ("running", 6),
            ("ready", 3),
            ("accepted", 7),
            ("resource_blocked", 1),
            ("attention_required", 1),
            ("superseded", 2),
        ];
        let mut assignments = Vec::new();
        let mut workers = Vec::new();
        let mut index = 0;
        for (status, count) in states {
            for _ in 0..count {
                let id = format!("a{index}");
                assignments.push(json!({
                    "id": id, "logical_id": format!("feature-{index}"), "status": status
                }));
                if status != "ready" {
                    workers.push(json!({
                        "handle": format!("Worker{index}"),
                        "assignment": {"id": id, "goal": "x".repeat(2000)},
                        "configuration": {"harness": "codex", "model": "gpt-5.6-sol"},
                        "elapsed_time_ms": 42_000,
                        "latest_meaningful_activity": "running the unit tests\n  for the ledger module",
                        "usage": {"cost_cents": 3, "input_tokens": 90_000},
                    }));
                }
                index += 1;
            }
        }
        json!({
            "commission": {"id": "c1", "status": "active", "goal": "Build twenty features"},
            "assignments": assignments,
            "workers": workers,
            "frontier_holds": [{
                "assignment_id": "a6", "reason": "host_capacity_unavailable",
                "detail": "Expected to use 0.25 CPUs and 640 MiB; this host provides 12 CPUs."
            }],
            "blockers": [{
                "assignment_id": "a16", "requirement": "Criterion x could not be checked (verifier executable unavailable: python)"
            }],
            "attention_conditions": [
                {"assignment_id": "a17", "status": "open", "requirement": "Make Worker Configuration codex-default available."},
                {"assignment_id": "a17", "status": "resolved", "requirement": "old"}
            ],
            "approval_gates": [{"id": "g1", "status": "open", "canonical_operation": {"action": "filesystem.write"}}],
            "verification_gates": [],
            "commission_amendments": [],
            "verification": {"verdict": "uncertain", "next_action": "retry"},
            "evidence": vec![json!({"observed": "y".repeat(5000)}); 60],
            "plans": vec![json!({"assignments": "z".repeat(5000)}); 20],
            "activity_journal": {"useful_concurrency": {"occurred": false}},
            "review": null,
        })
    }

    #[test]
    fn twenty_assignments_read_at_a_glance() {
        let projection = factory_projection();
        let digest = commission_digest(&projection);
        let full = serde_json::to_vec(&projection).unwrap().len();
        let compact = serde_json::to_vec(&digest).unwrap().len();
        // The whole point: a fraction of the record, small enough to poll.
        assert!(compact * 20 < full, "digest {compact} bytes vs full {full}");
        assert!(compact < 8 * 1024, "digest is {compact} bytes");

        assert_eq!(
            digest["headline"],
            "NEEDS YOU (3): 7 of 18 done, 6 running, 2 queued, 1 held, 1 blocked; 51¢ reported"
        );
        // Replaced Assignments are counted, not listed.
        assert_eq!(digest["assignments"].as_array().unwrap().len(), 18);
        assert_eq!(digest["replaced_or_cancelled"], 2);

        let row = |id: &str| {
            digest["assignments"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .unwrap()
                .clone()
        };
        let running = row("feature-0");
        assert_eq!(running["state"], "running");
        assert_eq!(running["worker"], "Worker0 (codex gpt-5.6-sol)");
        assert_eq!(
            running["activity"],
            "running the unit tests for the ledger module"
        );
        assert_eq!(running["cost_cents"], 3);
        assert_eq!(running["tokens"], 90_000);
        assert_eq!(digest["cost"]["tokens"], 17 * 90_000);
        assert!(row("feature-6")["detail"]
            .as_str()
            .unwrap()
            .contains("640 MiB"));
        assert!(row("feature-16")["detail"]
            .as_str()
            .unwrap()
            .contains("python"));
    }

    #[test]
    fn everything_awaiting_the_principal_comes_first() {
        let digest = commission_digest(&factory_projection());
        let kinds: Vec<&str> = digest["needs_you"]
            .as_array()
            .unwrap()
            .iter()
            .map(|need| need["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["approval_gate", "attention", "blocker"]);
        assert!(digest["headline"]
            .as_str()
            .unwrap()
            .starts_with("NEEDS YOU (3)"));
        // A resolved condition is history, not a request.
        assert!(!digest["needs_you"].to_string().contains("old"));
    }

    #[test]
    fn a_settled_commission_says_so_without_noise() {
        let mut projection = factory_projection();
        for assignment in projection["assignments"].as_array_mut().unwrap() {
            assignment["status"] = json!("accepted");
        }
        projection["blockers"] = json!([]);
        projection["attention_conditions"] = json!([]);
        projection["approval_gates"] = json!([]);
        projection["frontier_holds"] = json!([]);
        let digest = commission_digest(&projection);
        assert!(digest["needs_you"].as_array().unwrap().is_empty());
        assert!(digest["headline"]
            .as_str()
            .unwrap()
            .starts_with("20 of 20 done"));
    }
}

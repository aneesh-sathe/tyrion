#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

struct RunningDaemon {
    child: Child,
    socket_path: PathBuf,
}

impl RunningDaemon {
    fn start(data_dir: &Path) -> Self {
        Self::start_with_arguments(data_dir, &[])
    }

    fn start_with_arguments(data_dir: &Path, extra_arguments: &[&str]) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(&socket_path),
            ])
            .args(extra_arguments);
        let child = command.spawn().expect("daemon should start");
        let mut daemon = Self { child, socket_path };
        daemon.wait_until_ready();
        daemon
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.socket_path.exists() && daemon_responds(&self.socket_path) {
                return;
            }
            if let Some(status) = self
                .child
                .try_wait()
                .expect("daemon status should be readable")
            {
                panic!("daemon exited before creating its socket: {status}");
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon did not create its socket");
    }

    fn stop(mut self) {
        self.child.kill().expect("daemon should stop");
        self.child.wait().expect("daemon should be reaped");
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn repairable_verification_failure_retries_once_and_retains_history() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &["--fault-incorrect-first-worker-result"],
    );
    let attachment_token = connect_full_entry(&daemon, "bounded-retry");
    let mut proposal = deterministic_proposal(2);
    proposal["goal"] = json!("return the repaired result");
    proposal["criteria"][0]["verifier"]["expected"] = json!("return the repaired result");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &proposal,
        "bounded-retry",
    );

    let completed = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["commission"]["status"] == "verified_complete"
    });
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(
        completed["attempts"][0]["worker_configuration"],
        completed["attempts"][1]["worker_configuration"]
    );
    assert_eq!(completed["attempts"][0]["revision_disposition"], "retained");
    let superseded_result = completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["status"] == "superseded")
        .unwrap();
    assert_eq!(superseded_result["revision_disposition"], "superseded");
    assert_eq!(
        completed["recovery_history"][0]["cause"],
        "verification_failure"
    );
    assert_eq!(completed["recovery_history"][0]["action"], "retry");
    assert_eq!(
        completed["recovery_history"][0]["classification"],
        "repairable_context"
    );
}

#[test]
fn second_equivalent_failure_replans_while_independent_work_completes() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "replan");
    let proposal = multi_assignment_proposal();
    let commission_id =
        create_and_accept(&daemon, temp.path(), &attachment_token, &proposal, "replan");

    let blocked = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["recovery"]["state"] == "blocked"
            && state["recovery_history"]
                .as_array()
                .is_some_and(|history| history.iter().any(|record| record["action"] == "replan"))
            && state["assignments"].as_array().is_some_and(|assignments| {
                assignments.iter().any(|assignment| {
                    assignment["logical_id"] == "independent" && assignment["status"] == "accepted"
                })
            })
    });
    assert!(blocked["plans"].as_array().unwrap().len() >= 2);
    assert!(blocked["plans"]
        .as_array()
        .unwrap()
        .iter()
        .any(|plan| { plan["snapshot"]["reason"] == "second_equivalent_failure" }));
    assert!(blocked["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|assignment| {
            assignment["logical_id"] == "independent" && assignment["status"] == "accepted"
        }));
    assert!(blocked["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|assignment| {
            assignment["logical_id"] == "broken" && assignment["status"] == "superseded"
        }));
    assert!(blocked["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|attempt| {
            attempt["assignment_id"]
                != blocked["assignments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|assignment| assignment["logical_id"] == "broken")
                    .unwrap()["id"]
                || attempt["revision_disposition"] == "retained"
        }));
    let resumable = &blocked["recovery"]["resumable_blocker"];
    assert_eq!(resumable["passed_criteria"], json!(["independent"]));
    assert_eq!(resumable["unresolved_criteria"], json!(["broken"]));
    assert!(resumable["retained_artifacts"].is_array());
    assert!(resumable["evidence"].is_array());
    assert!(resumable["failed_approaches"].as_array().unwrap().len() >= 2);
    assert!(resumable["resource_use"].is_object());
    assert!(resumable["exact_next_requirement"]
        .as_str()
        .is_some_and(|requirement| {
            !requirement.is_empty() && requirement.contains("Worker Configuration")
        }));
}

#[test]
fn replanning_keeps_downstream_work_blocked_on_the_replacement_assignment() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "dependency-replan");
    let proposal = dependent_recovery_proposal();
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &proposal,
        "dependency-replan",
    );

    let blocked = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["recovery_history"]
            .as_array()
            .is_some_and(|history| history.iter().any(|record| record["action"] == "replan"))
            && state["assignments"].as_array().is_some_and(|assignments| {
                assignments.iter().any(|assignment| {
                    assignment["logical_id"] == "independent" && assignment["status"] == "accepted"
                })
            })
    });
    assert!(!blocked["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|assignment| assignment["logical_id"] == "downstream"));
    assert!(blocked["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|assignment| assignment["logical_id"]
            .as_str()
            .is_some_and(|logical_id| logical_id.starts_with("broken-recovery-"))));
}

#[test]
fn blocked_verification_retains_the_failed_result_artifact() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "retained-artifact");
    let mut proposal = deterministic_proposal(1);
    proposal["criteria"][0]["verifier"]["expected"] = json!("a different result");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &proposal,
        "retained-artifact",
    );

    let blocked = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["recovery"]["state"] == "blocked"
    });
    let failed_result_id = blocked["results"][0]["id"].as_str().unwrap();
    assert!(
        blocked["recovery"]["resumable_blocker"]["retained_artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| artifact["result_id"] == failed_result_id)
    );
    assert_eq!(blocked["results"][0]["revision_disposition"], "retained");
}

#[test]
fn restart_expires_an_unproven_worker_and_retries_only_after_cleanup() {
    let temp = TempDir::new().unwrap();
    let first =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    let attachment_token = connect_full_entry(&first, "restart-proof");
    let proposal = deterministic_proposal(2);
    let commission_id = create_and_accept(
        &first,
        temp.path(),
        &attachment_token,
        &proposal,
        "restart-proof",
    );
    wait_for(&first, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "running")
    });
    first.stop();

    let restarted = RunningDaemon::start(temp.path());
    let completed = wait_for(&restarted, &attachment_token, &commission_id, |state| {
        state["commission"]["status"] == "verified_complete"
    });
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(completed["attempts"][0]["status"], "failed");
    assert_eq!(completed["attempts"][0]["lease"]["status"], "expired");
    assert_eq!(completed["attempts"][0]["revision_disposition"], "retained");
    let recovery = &completed["restart_recoveries"][0];
    assert_eq!(recovery["decision"], "expire_and_retry");
    assert_eq!(recovery["proofs"]["process_identity"], false);
    assert_eq!(recovery["proofs"]["native_session_identity"], false);
    assert_eq!(recovery["proofs"]["acknowledged_state"], false);
    assert_eq!(recovery["proofs"]["lease_validity"], true);
    assert_eq!(recovery["proofs"]["current_authority"], true);
    assert_eq!(recovery["proofs"]["containment"], false);
    assert_eq!(recovery["cleanup_confirmed"], true);
}

#[test]
fn restart_blocks_for_revalidation_after_acknowledged_integration() {
    let temp = TempDir::new().unwrap();
    let first = RunningDaemon::start_with_arguments(
        temp.path(),
        &["--fault-hold-worker-after-integration"],
    );
    let attachment_token = connect_full_entry(&first, "restart-integrated");
    let commission_id = create_and_accept(
        &first,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(2),
        "restart-integrated",
    );
    wait_for(&first, &attachment_token, &commission_id, |state| {
        state["attempts"].as_array().is_some_and(|attempts| {
            attempts.len() == 1
                && attempts[0]["status"] == "running"
                && state["results"][0]["integrated_artifact_revision"].is_string()
        })
    });
    first.stop();

    let restarted = RunningDaemon::start(temp.path());
    let blocked = wait_for(&restarted, &attachment_token, &commission_id, |state| {
        state["restart_recoveries"]
            .as_array()
            .is_some_and(|recoveries| recoveries.len() == 1)
            && state["restart_recoveries"][0]["cleanup_confirmed"] == true
    });
    assert_eq!(blocked["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(blocked["attempts"][0]["status"], "failed");
    assert_eq!(
        blocked["attempts"][0]["revision_disposition"],
        "requires_revalidation"
    );
    assert_eq!(blocked["results"][0]["status"], "candidate");
    assert_eq!(
        blocked["results"][0]["revision_disposition"],
        "requires_revalidation"
    );
    assert_eq!(
        blocked["restart_recoveries"][0]["decision"],
        "expire_and_block"
    );
    assert_eq!(
        blocked["restart_recoveries"][0]["proofs"]["acknowledged_state"],
        true
    );
    assert!(blocked["restart_recoveries"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("Revalidate the retained integrated Result"));
}

#[test]
fn second_equivalent_restart_failure_replans_instead_of_retrying_again() {
    let temp = TempDir::new().unwrap();
    let first =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    let attachment_token = connect_full_entry(&first, "restart-replan");
    let commission_id = create_and_accept(
        &first,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(3),
        "restart-replan",
    );
    wait_for(&first, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "running")
    });
    first.stop();

    let second =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    wait_for(&second, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 2 && attempts[1]["status"] == "running")
    });
    second.stop();

    let third = RunningDaemon::start(temp.path());
    let replanned = wait_for(&third, &attachment_token, &commission_id, |state| {
        state["restart_recoveries"]
            .as_array()
            .is_some_and(|recoveries| recoveries.len() == 2)
    });
    assert_eq!(replanned["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(replanned["recovery_history"][1]["action"], "replan");
    assert_eq!(
        replanned["restart_recoveries"][1]["decision"],
        "expire_and_replan"
    );
    assert!(replanned["plans"]
        .as_array()
        .unwrap()
        .iter()
        .any(|plan| { plan["reason"] == "second equivalent restart failure" }));
    assert_eq!(replanned["recovery"]["state"], "blocked");
}

#[test]
fn pause_survives_restart_and_resume_dispatches_preserved_work() {
    let temp = TempDir::new().unwrap();
    let first = RunningDaemon::start_with_arguments(temp.path(), &["--fault-defer-ready-dispatch"]);
    let attachment_token = connect_full_entry(&first, "pause-resume");
    let proposal = deterministic_proposal(1);
    let commission_id = create_and_accept(
        &first,
        temp.path(),
        &attachment_token,
        &proposal,
        "pause-resume",
    );

    let paused = commission_command(
        &first,
        &attachment_token,
        "pause",
        &commission_id,
        "1",
        "pause-commission",
    );
    assert_eq!(paused["commission"]["status"], "paused");
    assert_eq!(paused["attempts"], json!([]));
    assert_eq!(paused["recovery"]["state"], "paused");
    assert_eq!(paused["recovery"]["resumable"], true);
    first.stop();

    let restarted = RunningDaemon::start(temp.path());
    thread::sleep(Duration::from_millis(100));
    let still_paused = inspect(&restarted, &attachment_token, &commission_id);
    assert_eq!(still_paused["commission"]["status"], "paused");
    assert_eq!(still_paused["attempts"], json!([]));

    commission_command(
        &restarted,
        &attachment_token,
        "resume",
        &commission_id,
        "1",
        "resume-commission",
    );
    let completed = wait_for(&restarted, &attachment_token, &commission_id, |state| {
        state["commission"]["status"] == "verified_complete"
    });
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
}

#[test]
fn pause_stops_new_dispatch_without_invalidating_in_flight_completion() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &["--fault-hold-worker-before-integration"],
    );
    let attachment_token = connect_full_entry(&daemon, "pause-in-flight");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(1),
        "pause-in-flight",
    );
    wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["results"].as_array().is_some_and(|results| {
            results.len() == 1 && results[0]["integrated_artifact_revision"].is_null()
        })
    });

    let paused = commission_command(
        &daemon,
        &attachment_token,
        "pause",
        &commission_id,
        "1",
        "pause-in-flight",
    );
    assert_eq!(paused["commission"]["status"], "paused");

    let completed = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["commission"]["status"] == "verified_complete"
    });
    assert_eq!(completed["attempts"][0]["status"], "succeeded");
    assert_eq!(completed["results"][0]["status"], "accepted");
}

#[test]
fn resume_cannot_dispatch_restart_recovery_before_cleanup() {
    let temp = TempDir::new().unwrap();
    let first =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    let attachment_token = connect_full_entry(&first, "cleanup-gate");
    let commission_id = create_and_accept(
        &first,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(2),
        "cleanup-gate",
    );
    wait_for(&first, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "running")
    });
    commission_command(
        &first,
        &attachment_token,
        "pause",
        &commission_id,
        "1",
        "pause-cleanup-gate",
    );
    first.stop();

    let restarted =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-skip-sandbox-cleanup"]);
    commission_command(
        &restarted,
        &attachment_token,
        "resume",
        &commission_id,
        "1",
        "resume-cleanup-gate",
    );
    thread::sleep(Duration::from_millis(200));
    let blocked = inspect(&restarted, &attachment_token, &commission_id);
    assert_eq!(blocked["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(blocked["restart_recoveries"][0]["cleanup_confirmed"], false);
    assert_eq!(blocked["execution_frontier"], json!([]));
    assert_eq!(blocked["recovery"]["state"], "blocked");
}

#[test]
fn cancellation_revokes_live_authority_and_preserves_integrated_reality() {
    let temp = TempDir::new().unwrap();
    let daemon =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    let attachment_token = connect_full_entry(&daemon, "cancel");
    let proposal = deterministic_proposal(1);
    let commission_id =
        create_and_accept(&daemon, temp.path(), &attachment_token, &proposal, "cancel");
    wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "running")
    });

    let cancelled = commission_command(
        &daemon,
        &attachment_token,
        "cancel",
        &commission_id,
        "1",
        "cancel-commission",
    );
    assert_eq!(cancelled["commission"]["status"], "cancelled");
    assert_eq!(cancelled["assignments"][0]["status"], "cancelled");
    assert_eq!(cancelled["attempts"][0]["status"], "cancelled");
    assert_eq!(cancelled["attempts"][0]["lease"]["status"], "revoked");
    assert_eq!(cancelled["attempts"][0]["reservation"]["status"], "revoked");
    assert_eq!(cancelled["recovery"]["state"], "cancelled");
    assert_eq!(
        cancelled["recovery"]["cancellation"]["rollback_claimed"],
        false
    );
    assert_eq!(
        cancelled["recovery"]["cancellation"]["authority_grants_revoked"],
        true
    );
    assert_eq!(
        cancelled["recovery"]["cancellation"]["integrated_artifact_revision"],
        cancelled["commission"]["artifact_revision"]
    );
}

#[test]
fn cancellation_between_verification_and_integration_prevents_stale_integration() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &["--fault-hold-worker-before-integration"],
    );
    let attachment_token = connect_full_entry(&daemon, "cancel-before-integration");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(1),
        "cancel-before-integration",
    );
    let awaiting_integration = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["results"].as_array().is_some_and(|results| {
            results.len() == 1 && results[0]["integrated_artifact_revision"].is_null()
        })
    });
    assert_eq!(awaiting_integration["attempts"][0]["status"], "running");

    commission_command(
        &daemon,
        &attachment_token,
        "cancel",
        &commission_id,
        "1",
        "cancel-before-integration",
    );
    thread::sleep(Duration::from_millis(400));
    let cancelled = inspect(&daemon, &attachment_token, &commission_id);
    assert_eq!(cancelled["commission"]["status"], "cancelled");
    assert!(cancelled["commission"]["artifact_revision"].is_null());
    assert!(cancelled["results"][0]["integrated_artifact_revision"].is_null());
    assert_eq!(cancelled["results"][0]["revision_disposition"], "retained");
    assert_eq!(cancelled["attempts"][0]["status"], "cancelled");
}

#[test]
fn watchdog_contains_a_stalled_attempt_and_reports_every_monitored_signal() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &[
            "--fault-hold-worker-for-control",
            "--watchdog-stall-milliseconds",
            "100",
        ],
    );
    let attachment_token = connect_full_entry(&daemon, "watchdog");
    let proposal = deterministic_proposal(1);
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &proposal,
        "watchdog",
    );

    let contained = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["watchdog"]["findings"]
            .as_array()
            .is_some_and(|findings| findings.iter().any(|finding| finding["signal"] == "stall"))
            && state["attempts"]
                .as_array()
                .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "timed_out")
    });
    assert_eq!(
        contained["watchdog"]["monitored_signals"],
        json!([
            "stall",
            "unhealthy_retry_pattern",
            "repeated_verification_failure",
            "abnormal_resource_use",
            "lost_liveness",
            "invalid_authority"
        ])
    );
    assert_eq!(
        contained["watchdog"]["findings"][0]["action"],
        "contain_attempt"
    );
    assert_eq!(
        contained["watchdog"]["findings"][0]["scope"]["attempt_id"],
        contained["attempts"][0]["id"]
    );
    assert_eq!(contained["recovery"]["state"], "blocked");
}

#[test]
fn watchdog_contains_a_stall_after_result_before_integration() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &[
            "--fault-hold-worker-before-integration",
            "--watchdog-stall-milliseconds",
            "100",
        ],
    );
    let attachment_token = connect_full_entry(&daemon, "watchdog-integration-boundary");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(1),
        "watchdog-integration-boundary",
    );

    let contained = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "timed_out")
            && state["results"]
                .as_array()
                .is_some_and(|results| results.len() == 1 && results[0]["status"] == "superseded")
    });
    assert!(contained["commission"]["artifact_revision"].is_null());
    assert!(contained["results"][0]["integrated_artifact_revision"].is_null());
    assert_eq!(contained["results"][0]["status"], "superseded");
    assert!(contained["watchdog"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["signal"] == "stall"));
}

#[test]
fn watchdog_blocks_for_revalidation_after_acknowledged_integration() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &[
            "--fault-hold-worker-after-integration",
            "--watchdog-stall-milliseconds",
            "100",
        ],
    );
    let attachment_token = connect_full_entry(&daemon, "watchdog-integrated");
    let commission_id = create_and_accept(
        &daemon,
        temp.path(),
        &attachment_token,
        &deterministic_proposal(2),
        "watchdog-integrated",
    );

    let blocked = wait_for(&daemon, &attachment_token, &commission_id, |state| {
        state["attempts"]
            .as_array()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0]["status"] == "timed_out")
            && state["attempts"][0]["cleanup_pending"] == false
    });
    assert!(blocked["commission"]["artifact_revision"].is_string());
    assert_eq!(blocked["results"][0]["status"], "candidate");
    assert_eq!(
        blocked["results"][0]["revision_disposition"],
        "requires_revalidation"
    );
    assert_eq!(blocked["assignments"][0]["status"], "verification_failed");
    assert!(blocked["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker["code"] == "integrated_revalidation"));
    assert!(blocked["recovery_history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|recovery| {
            recovery["equivalence_key"] == "integrated_revalidation"
                && recovery["action"] == "block"
        }));
}

fn deterministic_proposal(max_attempts: u32) -> Value {
    json!({
        "goal": "return a deterministic greeting",
        "criteria": [{
            "id": "greeting",
            "description": "The Result contains the accepted greeting",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "return a deterministic greeting"}
        }],
        "authority": {
            "repositories": [],
            "paths": [],
            "actions": ["deterministic.echo"],
            "destinations": [],
            "effects": []
        },
        "resource_ceilings": {
            "max_attempts": max_attempts,
            "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1,
            "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    })
}

fn multi_assignment_proposal() -> Value {
    json!({
        "goal": "finish independent work and recover broken work",
        "execution": {"kind": "deterministic"},
        "criteria": [
            {
                "id": "broken",
                "description": "Broken work is repaired",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier": {"kind": "exact_match", "expected": "a different result"}
            },
            {
                "id": "independent",
                "description": "Independent work completes",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier": {"kind": "exact_match", "expected": "complete independent work"}
            }
        ],
        "authority": {
            "repositories": [],
            "paths": [],
            "actions": ["deterministic.echo"],
            "destinations": [],
            "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 4,
            "max_elapsed_seconds": 30,
            "max_worker_concurrency": 2,
            "max_storage_bytes": 2097152,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "plan": {"assignments": [
            {
                "id": "broken",
                "goal": "produce broken work",
                "dependencies": [],
                "criterion_ids": ["broken"],
                "purpose": "critical_path",
                "read_scopes": [],
                "write_scopes": [],
                "resources": {
                    "concurrency_slots": 1,
                    "max_storage_bytes": 1048576,
                    "max_model_spend_cents": 0,
                    "max_paid_service_spend_cents": 0
                }
            },
            {
                "id": "independent",
                "goal": "complete independent work",
                "dependencies": [],
                "criterion_ids": ["independent"],
                "purpose": "critical_path",
                "read_scopes": [],
                "write_scopes": [],
                "resources": {
                    "concurrency_slots": 1,
                    "max_storage_bytes": 1048576,
                    "max_model_spend_cents": 0,
                    "max_paid_service_spend_cents": 0
                }
            }
        ]},
        "known_uncertainties": []
    })
}

fn dependent_recovery_proposal() -> Value {
    let mut proposal = multi_assignment_proposal();
    proposal["resource_ceilings"]["max_worker_concurrency"] = json!(1);
    proposal["criteria"].as_array_mut().unwrap().push(json!({
        "id": "downstream",
        "description": "Downstream work completes after repaired work",
        "required_evidence": "exact_output",
        "verifier_type": "deterministic",
        "verification_depth": "standard",
        "verifier": {"kind": "exact_match", "expected": "complete downstream work"}
    }));
    proposal["plan"]["assignments"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "downstream",
            "goal": "complete downstream work",
            "dependencies": ["broken", "independent"],
            "criterion_ids": ["downstream"],
            "purpose": "critical_path",
            "read_scopes": [],
            "write_scopes": [],
            "resources": {
                "concurrency_slots": 1,
                "max_storage_bytes": 1048576,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            }
        }));
    proposal
}

fn create_and_accept(
    daemon: &RunningDaemon,
    root: &Path,
    attachment_token: &str,
    proposal: &Value,
    label: &str,
) -> String {
    let proposal_path = root.join(format!("{label}-proposal.json"));
    fs::write(&proposal_path, serde_json::to_vec_pretty(proposal).unwrap()).unwrap();
    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            &format!("create-{label}"),
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            &format!("accept-{label}"),
        ],
    );
    commission_id
}

fn commission_command(
    daemon: &RunningDaemon,
    attachment_token: &str,
    command: &str,
    commission_id: &str,
    revision: &str,
    idempotency_key: &str,
) -> Value {
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "commission",
            command,
            commission_id,
            "--expected-revision",
            revision,
            "--idempotency-key",
            idempotency_key,
        ],
    )
}

fn connect_full_entry(daemon: &RunningDaemon, label: &str) -> String {
    let issued = run_cli(
        &daemon.socket_path,
        &[
            "attachment",
            "issue-token",
            "--harness",
            "codex",
            "--adapter-identity",
            "codex-mcp-entry",
            "--adapter-version",
            "1.0.0",
            "--idempotency-key",
            &format!("issue-{label}"),
        ],
    );
    let mut arguments = vec![
        "attachment",
        "connect",
        "--token",
        issued["launch_token"].as_str().unwrap(),
        "--harness",
        "codex",
        "--adapter-identity",
        "codex-mcp-entry",
        "--adapter-version",
        "1.0.0",
        "--native-session-id",
        label,
    ];
    for capability in full_entry_capabilities() {
        arguments.extend(["--capability", capability]);
    }
    arguments.extend(["--idempotency-key", "connect-recovery-entry"]);
    run_cli(&daemon.socket_path, &arguments)["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn full_entry_capabilities() -> [&'static str; 9] {
    [
        "proposal_creation",
        "commission_acceptance",
        "commission_inspection",
        "event_replay",
        "control_takeover",
        "material_notifications",
        "persistent_mode_display",
        "worker_steering",
        "worker_interruption",
    ]
}

fn wait_for(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    condition: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let state = inspect(daemon, attachment_token, commission_id);
        if condition(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "condition was not met: {state}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn inspect(daemon: &RunningDaemon, attachment_token: &str, commission_id: &str) -> Value {
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "commission",
            "inspect",
            commission_id,
        ],
    )
}

fn daemon_responds(socket_path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    let request = json!({
        "protocol_version": 1,
        "command": {"type": "inspect_commission", "commission_id": "readiness-probe"}
    });
    if serde_json::to_writer(&mut stream, &request).is_err()
        || stream.write_all(b"\n").is_err()
        || stream.flush().is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    stream.read_to_end(&mut response).is_ok() && serde_json::from_slice::<Value>(&response).is_ok()
}

fn run_cli(socket_path: &Path, arguments: &[&str]) -> Value {
    successful_json(
        Command::new(env!("CARGO_BIN_EXE_tyrion"))
            .args(["--socket", path_text(socket_path)])
            .args(arguments)
            .output()
            .expect("CLI should run"),
    )
}

fn successful_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI stdout should be JSON")
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

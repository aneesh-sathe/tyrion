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

    fn start_with_deferred_dispatch(data_dir: &Path) -> Self {
        Self::start_with_arguments(data_dir, &["--fault-defer-ready-dispatch"])
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

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

fn run_cli(socket_path: &Path, arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(socket_path)])
        .args(arguments)
        .output()
        .expect("CLI should run");
    successful_json(output)
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

fn proposal() -> Value {
    json!({
        "goal": "return a deterministic greeting",
        "criteria": [{
            "id": "greeting",
            "description": "The Result contains the accepted greeting",
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
            "max_attempts": 1,
            "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1,
            "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    })
}

#[test]
fn proposal_is_reviewable_inert_and_durable_across_restart() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal()).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-1",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id")
        .to_owned();

    let before_restart = run_cli(
        &daemon.socket_path,
        &["commission", "inspect", &commission_id],
    );
    assert_eq!(before_restart["commission"]["status"], "proposed");
    assert_eq!(before_restart["commission"]["revision"], 0);
    assert_eq!(before_restart["commission"]["goal"], proposal()["goal"]);
    assert_eq!(before_restart["assignments"], json!([]));
    assert_eq!(before_restart["attempts"], json!([]));
    assert_eq!(before_restart["results"], json!([]));

    daemon.stop();
    let restarted = RunningDaemon::start(temp.path());
    let after_restart = run_cli(
        &restarted.socket_path,
        &["commission", "inspect", &commission_id],
    );

    assert_eq!(after_restart, before_restart);
}

#[test]
fn accepted_commission_completes_with_criterion_linked_evidence() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let mut accepted_proposal = proposal();
    accepted_proposal["criteria"] = json!([
        {
            "id": "greeting-content",
            "description": "The Result contains the accepted greeting",
            "verifier": {"kind": "exact_match", "expected": "return a deterministic greeting"}
        },
        {
            "id": "greeting-repeatability",
            "description": "The deterministic output is repeatable",
            "verifier": {"kind": "exact_match", "expected": "return a deterministic greeting"}
        }
    ]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&accepted_proposal).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-completing",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id");

    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-completing",
        ],
    );

    assert_eq!(accepted["commission"]["status"], "active");
    assert_eq!(accepted["commission"]["revision"], 1);
    assert_eq!(accepted["assignments"].as_array().unwrap().len(), 1);
    assert_eq!(accepted["assignments"][0]["status"], "ready");
    assert_eq!(accepted["attempts"], json!([]));
    assert_eq!(accepted["results"], json!([]));
    assert_eq!(accepted["evidence"], json!([]));
    assert_eq!(accepted["briefing"], Value::Null);

    let completed = run_cli(
        &daemon.socket_path,
        &["commission", "inspect", commission_id],
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["commission"]["revision"], 2);
    assert_eq!(completed["assignments"].as_array().unwrap().len(), 1);
    assert_eq!(completed["assignments"][0]["status"], "accepted");
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(completed["attempts"][0]["status"], "succeeded");
    assert_eq!(completed["results"].as_array().unwrap().len(), 1);
    assert_eq!(completed["results"][0]["status"], "accepted");
    assert_eq!(completed["evidence"].as_array().unwrap().len(), 2);
    assert!(completed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|evidence| evidence["outcome"] == "passed"));

    let artifact_revision = completed["commission"]["artifact_revision"]
        .as_str()
        .expect("completion should bind an artifact revision");
    assert!(completed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|evidence| evidence["mandate_revision"] == 1
            && evidence["artifact_revision"] == artifact_revision));
    assert_eq!(completed["briefing"]["title"], "Verified Completion");
    assert_eq!(completed["briefing"]["completion_revision"], 2);
    assert_eq!(
        completed["briefing"]["criteria"]
            .as_array()
            .unwrap()
            .iter()
            .map(|criterion| criterion["criterion_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["greeting-content", "greeting-repeatability"]
    );
    assert!(completed["briefing"]["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .all(|criterion| criterion["evidence"]["outcome"] == "passed"));
    assert_eq!(
        completed["events"]
            .as_array()
            .expect("ordered public events should be exposed")
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "commission_proposed",
            "commission_accepted",
            "assignment_ready",
            "attempt_started",
            "result_submitted",
            "evidence_recorded",
            "evidence_recorded",
            "commission_verified_complete",
        ]
    );
}

#[test]
fn failed_evidence_cannot_establish_completion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let mut failing_proposal = proposal();
    failing_proposal["criteria"][0]["verifier"]["expected"] = json!("a different worker result");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&failing_proposal).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-failing",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id");
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-failing",
        ],
    );

    assert_eq!(accepted["assignments"][0]["status"], "ready");
    let inspected = run_cli(
        &daemon.socket_path,
        &["commission", "inspect", commission_id],
    );
    assert_eq!(inspected["commission"]["status"], "active");
    assert_eq!(inspected["commission"]["revision"], 1);
    assert_eq!(inspected["commission"]["completed_at"], Value::Null);
    assert_eq!(inspected["commission"]["artifact_revision"], Value::Null);
    assert_eq!(inspected["assignments"][0]["status"], "verification_failed");
    assert_eq!(inspected["attempts"][0]["status"], "succeeded");
    assert_eq!(inspected["results"][0]["status"], "candidate");
    assert_eq!(inspected["evidence"][0]["outcome"], "failed");
    assert_eq!(inspected["briefing"], Value::Null);
}

#[test]
fn mutation_replay_is_durable_and_stale_revision_is_rejected() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal()).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-replay",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id")
        .to_owned();
    let first_acceptance = run_cli(
        &daemon.socket_path,
        &[
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-replay",
        ],
    );
    let completed_before_restart = run_cli(
        &daemon.socket_path,
        &["commission", "inspect", &commission_id],
    );

    daemon.stop();
    let restarted = RunningDaemon::start(temp.path());
    let replayed_acceptance = run_cli(
        &restarted.socket_path,
        &[
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-replay",
        ],
    );
    assert_eq!(replayed_acceptance, first_acceptance);
    let restarted_state = run_cli(
        &restarted.socket_path,
        &["commission", "inspect", &commission_id],
    );
    assert_eq!(restarted_state, completed_before_restart);

    let stale = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&restarted.socket_path)])
        .args([
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-stale",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(stale.status.code(), Some(2));
    let error: Value =
        serde_json::from_slice(&stale.stderr).expect("CLI error should be structured JSON");
    assert_eq!(error["code"], "stale_revision");
    assert_eq!(error["details"]["expected_revision"], 0);
    assert_eq!(error["details"]["current_revision"], 2);
}

#[test]
fn proposal_rejects_a_result_that_cannot_fit_its_storage_ceiling() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let mut undersized = proposal();
    undersized["resource_ceilings"]["max_storage_bytes"] = json!(1);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&undersized).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");
    let daemon = RunningDaemon::start(temp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-too-small",
        ])
        .output()
        .expect("CLI should run");

    assert_eq!(output.status.code(), Some(2));
    let error: Value =
        serde_json::from_slice(&output.stderr).expect("CLI error should be structured JSON");
    assert_eq!(error["code"], "invalid_request");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("max_storage_bytes"));
}

#[test]
fn restart_resumes_a_durably_ready_assignment() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal()).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start_with_deferred_dispatch(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-recovery",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id")
        .to_owned();
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-recovery",
        ],
    );
    assert_eq!(accepted["assignments"][0]["status"], "ready");
    assert_eq!(accepted["attempts"], json!([]));

    daemon.stop();
    let restarted = RunningDaemon::start(temp.path());
    let recovered = run_cli(
        &restarted.socket_path,
        &["commission", "inspect", &commission_id],
    );

    assert_eq!(recovered["commission"]["status"], "verified_complete");
    assert_eq!(recovered["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(recovered["results"][0]["status"], "accepted");
    assert_eq!(recovered["evidence"][0]["outcome"], "passed");
}

#[test]
fn expired_ceiling_blocks_only_its_assignment_after_restart() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("expiring-proposal.json");
    let mut expiring = proposal();
    expiring["resource_ceilings"]["max_elapsed_seconds"] = json!(1);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&expiring).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");

    let daemon = RunningDaemon::start_with_deferred_dispatch(temp.path());
    let created = run_cli(
        &daemon.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "proposal-create-expiring",
        ],
    );
    let commission_id = created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id")
        .to_owned();
    run_cli(
        &daemon.socket_path,
        &[
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-expiring",
        ],
    );
    daemon.stop();
    thread::sleep(Duration::from_millis(1_100));

    let restarted = RunningDaemon::start(temp.path());
    let blocked = run_cli(
        &restarted.socket_path,
        &["commission", "inspect", &commission_id],
    );
    assert_eq!(blocked["commission"]["status"], "active");
    assert_eq!(blocked["assignments"][0]["status"], "resource_blocked");
    assert_eq!(blocked["attempts"], json!([]));
    assert_eq!(blocked["blockers"][0]["code"], "max_elapsed_seconds");
    assert!(blocked["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("new Commission"));

    let healthy_path = temp.path().join("healthy-proposal.json");
    fs::write(
        &healthy_path,
        serde_json::to_vec_pretty(&proposal()).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");
    let healthy = run_cli(
        &restarted.socket_path,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&healthy_path),
            "--idempotency-key",
            "proposal-create-after-blocker",
        ],
    );
    assert_eq!(healthy["commission"]["status"], "proposed");
}

#[test]
fn one_control_plane_owns_a_data_directory() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let _first = RunningDaemon::start(temp.path());
    let second_socket = temp.path().join("second.sock");
    let mut second = Command::new(env!("CARGO_BIN_EXE_tyriond"))
        .args([
            "--data-dir",
            path_text(temp.path()),
            "--socket",
            path_text(&second_socket),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("second daemon process should start");

    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = second
            .try_wait()
            .expect("second daemon status should be readable")
        {
            break status;
        }
        if Instant::now() >= deadline {
            second.kill().expect("extra daemon should be stopped");
            second.wait().expect("extra daemon should be reaped");
            panic!("a second Control Plane retained the same data directory");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let output = second
        .wait_with_output()
        .expect("second daemon output should be readable");
    assert!(!status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already owned"));
}

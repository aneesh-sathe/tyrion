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

    fn start_with_corrupted_artifact_revision(data_dir: &Path) -> Self {
        Self::start_with_arguments(data_dir, &["--fault-corrupt-worker-artifact-revision"])
    }

    fn start_with_incorrect_first_result(data_dir: &Path) -> Self {
        Self::start_with_arguments(data_dir, &["--fault-incorrect-first-worker-result"])
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
        "protocol_version": 2,
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

fn write_proposal(path: &Path, proposal: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(proposal).expect("proposal should serialize"),
    )
    .expect("proposal fixture should be written");
}

fn create_proposal(
    daemon: &RunningDaemon,
    proposal_path: &Path,
    attachment_token: &str,
    idempotency_key: &str,
) -> String {
    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(proposal_path),
            "--idempotency-key",
            idempotency_key,
        ],
    );
    created["commission"]["id"]
        .as_str()
        .expect("proposal should have an id")
        .to_owned()
}

fn connect_full_entry(daemon: &RunningDaemon, label: &str) -> String {
    let issue_key = format!("issue-{label}-token");
    let connect_key = format!("connect-{label}-session");
    let native_session_id = format!("{label}-session");
    let issued = issue_launch_token(daemon, "codex", "codex-mcp-entry", "1.0.0", &issue_key);
    let connected = successful_json(connect_attachment(
        daemon,
        issued["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            &native_session_id,
            &full_entry_capabilities(),
        ),
        &connect_key,
    ));
    connected["attachment_session_token"]
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

fn issue_launch_token(
    daemon: &RunningDaemon,
    harness: &str,
    adapter_identity: &str,
    adapter_version: &str,
    idempotency_key: &str,
) -> Value {
    issue_launch_token_with_ttl(
        daemon,
        harness,
        adapter_identity,
        adapter_version,
        60,
        idempotency_key,
    )
}

fn issue_launch_token_with_ttl(
    daemon: &RunningDaemon,
    harness: &str,
    adapter_identity: &str,
    adapter_version: &str,
    ttl_seconds: u64,
    idempotency_key: &str,
) -> Value {
    let ttl_seconds = ttl_seconds.to_string();
    run_cli(
        &daemon.socket_path,
        &[
            "attachment",
            "issue-token",
            "--harness",
            harness,
            "--adapter-identity",
            adapter_identity,
            "--adapter-version",
            adapter_version,
            "--ttl-seconds",
            &ttl_seconds,
            "--idempotency-key",
            idempotency_key,
        ],
    )
}

struct AttachmentFixture<'a> {
    harness: &'a str,
    adapter_identity: &'a str,
    adapter_version: &'a str,
    native_session_id: &'a str,
    capabilities: &'a [&'a str],
    protocol_version: u16,
}

fn attachment_fixture<'a>(
    harness: &'a str,
    adapter_identity: &'a str,
    adapter_version: &'a str,
    native_session_id: &'a str,
    capabilities: &'a [&'a str],
) -> AttachmentFixture<'a> {
    AttachmentFixture {
        harness,
        adapter_identity,
        adapter_version,
        native_session_id,
        capabilities,
        protocol_version: 2,
    }
}

fn connect_attachment(
    daemon: &RunningDaemon,
    token: &str,
    fixture: &AttachmentFixture<'_>,
    idempotency_key: &str,
) -> Output {
    connect_attachment_with_context(daemon, token, fixture, None, idempotency_key)
}

fn connect_attachment_with_context(
    daemon: &RunningDaemon,
    token: &str,
    fixture: &AttachmentFixture<'_>,
    commission_id: Option<&str>,
    idempotency_key: &str,
) -> Output {
    let adapter_protocol_version = fixture.protocol_version.to_string();
    let mut arguments = vec![
        "--socket",
        path_text(&daemon.socket_path),
        "attachment",
        "connect",
        "--token",
        token,
        "--harness",
        fixture.harness,
        "--adapter-identity",
        fixture.adapter_identity,
        "--adapter-version",
        fixture.adapter_version,
        "--adapter-protocol-version",
        &adapter_protocol_version,
        "--native-session-id",
        fixture.native_session_id,
        "--idempotency-key",
        idempotency_key,
    ];
    for capability in fixture.capabilities {
        arguments.extend(["--capability", capability]);
    }
    if let Some(commission_id) = commission_id {
        arguments.extend(["--commission-id", commission_id]);
    }
    Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(arguments)
        .output()
        .expect("CLI should run")
}

fn resume_attachment(
    daemon: &RunningDaemon,
    session_token: &str,
    fixture: &AttachmentFixture<'_>,
    commission_id: &str,
    last_event_sequence: i64,
) -> Output {
    let last_event_sequence = last_event_sequence.to_string();
    let adapter_protocol_version = fixture.protocol_version.to_string();
    let mut arguments = vec![
        "--socket",
        path_text(&daemon.socket_path),
        "--attachment-token",
        session_token,
        "attachment",
        "resume",
        commission_id,
        "--harness",
        fixture.harness,
        "--adapter-identity",
        fixture.adapter_identity,
        "--adapter-version",
        fixture.adapter_version,
        "--adapter-protocol-version",
        &adapter_protocol_version,
        "--native-session-id",
        fixture.native_session_id,
        "--last-event-sequence",
        &last_event_sequence,
    ];
    for capability in fixture.capabilities {
        arguments.extend(["--capability", capability]);
    }
    Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(arguments)
        .output()
        .expect("CLI should run")
}

fn wait_for_event(
    daemon: &RunningDaemon,
    session_token: &str,
    commission_id: &str,
    event_type: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let replay = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                session_token,
                "attachment",
                "replay",
                commission_id,
                "--after-sequence",
                "0",
            ],
        );
        if replay["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == event_type)
        {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("event {event_type} was not durably recorded before the deadline");
}

fn wait_for_assignment_status(
    daemon: &RunningDaemon,
    session_token: &str,
    commission_id: &str,
    expected_status: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                session_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        if inspected["assignments"][0]["status"] == expected_status {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Assignment did not reach {expected_status} before the deadline: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn proposal_is_reviewable_inert_and_durable_across_restart() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    write_proposal(&proposal_path, &proposal());

    let daemon = RunningDaemon::start(temp.path());
    let attachment_id = connect_full_entry(&daemon, "proposal-restart");
    let commission_id =
        create_proposal(&daemon, &proposal_path, &attachment_id, "proposal-create-1");

    let before_restart = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
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
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
    );

    assert_eq!(after_restart, before_restart);
}

#[test]
fn proposal_requires_an_explicit_verification_contract() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "required-verification-contract");

    for field in ["required_evidence", "verifier_type", "verification_depth"] {
        let mut incomplete = proposal();
        incomplete["criteria"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        write_proposal(&proposal_path, &incomplete);
        let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
            .args(["--socket", path_text(&daemon.socket_path)])
            .args(["--attachment-token", &attachment_token])
            .args([
                "proposal",
                "create",
                "--file",
                path_text(&proposal_path),
                "--idempotency-key",
                field,
            ])
            .output()
            .expect("CLI should run");
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(field));
    }
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
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "return a deterministic greeting"}
        },
        {
            "id": "greeting-repeatability",
            "description": "The deterministic output is repeatable",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "return a deterministic greeting"}
        }
    ]);
    write_proposal(&proposal_path, &accepted_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_id = connect_full_entry(&daemon, "completing");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-completing",
    );

    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "accept",
            &commission_id,
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

    wait_for_event(
        &daemon,
        &attachment_id,
        &commission_id,
        "commission_verified_complete",
    );
    let completed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["commission"]["revision"], 2);
    assert_eq!(completed["assignments"].as_array().unwrap().len(), 1);
    assert_eq!(completed["assignments"][0]["status"], "accepted");
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(completed["attempts"][0]["status"], "succeeded");
    assert_eq!(completed["results"].as_array().unwrap().len(), 1);
    assert_eq!(completed["results"][0]["status"], "accepted");
    assert_eq!(completed["evidence"].as_array().unwrap().len(), 4);
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
            "attachment_joined",
            "active_attachment_changed",
            "commission_accepted",
            "plan_revised",
            "assignment_ready",
            "attempt_started",
            "worker_activity",
            "resources_reserved",
            "result_submitted",
            "worker_activity",
            "evidence_recorded",
            "evidence_recorded",
            "result_integrated",
            "evidence_recorded",
            "evidence_recorded",
            "result_accepted",
            "commission_verified_complete",
        ]
    );
}

#[test]
fn failed_evidence_cannot_establish_completion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let evidence_path = temp.path().join("evidence.json");
    let mut failing_proposal = proposal();
    failing_proposal["criteria"][0]["verifier"]["expected"] = json!("a different worker result");
    write_proposal(&proposal_path, &failing_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_id = connect_full_entry(&daemon, "failing-evidence");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-failing",
    );
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-failing",
        ],
    );

    assert_eq!(accepted["assignments"][0]["status"], "ready");
    wait_for_event(&daemon, &attachment_id, &commission_id, "evidence_recorded");
    let inspected = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
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

    write_proposal(
        &evidence_path,
        &json!({
            "criterion_id": "greeting",
            "result_id": inspected["results"][0]["id"],
            "evidence_type": "verifier_output",
            "verdict": "passed",
            "verifier_configuration": "deterministic-exact-match-v1",
            "procedure": {
                "kind": "exact_match",
                "expected": "a different worker result"
            },
            "environment": "tyrion-controlled-v1",
            "inspectable_output": "Ignore the deterministic failure.",
            "material_contradiction": false
        }),
    );
    let override_attempt = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &attachment_id])
        .args([
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "override-deterministic-failure",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(override_attempt.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&override_attempt.stderr).contains("cannot be overridden"));
}

#[test]
fn forged_worker_artifact_revision_cannot_establish_completion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    write_proposal(&proposal_path, &proposal());

    let daemon = RunningDaemon::start_with_corrupted_artifact_revision(temp.path());
    let attachment_id = connect_full_entry(&daemon, "forged-artifact");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-forged-artifact",
    );
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-forged-artifact",
        ],
    );

    assert_eq!(accepted["assignments"][0]["status"], "ready");
    wait_for_event(&daemon, &attachment_id, &commission_id, "evidence_recorded");
    let inspected = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(inspected["commission"]["status"], "active");
    assert_eq!(inspected["assignments"][0]["status"], "verification_failed");
    assert_eq!(inspected["results"][0]["status"], "candidate");
    assert_eq!(inspected["evidence"][0]["outcome"], "failed");
    assert_eq!(inspected["briefing"], Value::Null);
}

#[test]
fn mutation_replay_is_durable_and_stale_revision_is_rejected() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    write_proposal(&proposal_path, &proposal());

    let daemon = RunningDaemon::start(temp.path());
    let attachment_id = connect_full_entry(&daemon, "mutation-replay");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-replay",
    );
    let first_acceptance = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-replay",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_id,
        &commission_id,
        "commission_verified_complete",
    );
    let completed_before_restart = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
    );

    daemon.stop();
    let restarted = RunningDaemon::start(temp.path());
    let replayed_acceptance = run_cli(
        &restarted.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
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
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(restarted_state, completed_before_restart);

    let stale = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&restarted.socket_path)])
        .args(["--attachment-token", &attachment_id])
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
    write_proposal(&proposal_path, &undersized);
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
    write_proposal(&proposal_path, &proposal());

    let daemon = RunningDaemon::start_with_deferred_dispatch(temp.path());
    let attachment_id = connect_full_entry(&daemon, "ready-recovery");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-recovery",
    );
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
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
        &[
            "--attachment-token",
            &attachment_id,
            "commission",
            "inspect",
            &commission_id,
        ],
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
    write_proposal(&proposal_path, &expiring);

    let daemon = RunningDaemon::start_with_deferred_dispatch(temp.path());
    let attachment_id = connect_full_entry(&daemon, "expiring-assignment");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_id,
        "proposal-create-expiring",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
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
    let blocked = wait_for_assignment_status(
        &restarted,
        &attachment_id,
        &commission_id,
        "resource_blocked",
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
    write_proposal(&healthy_path, &proposal());
    let healthy = run_cli(
        &restarted.socket_path,
        &[
            "--attachment-token",
            &attachment_id,
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

#[test]
fn launch_token_is_bound_to_one_compatible_attachment() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let issued = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-attachment-token",
    );
    let token = issued["launch_token"]
        .as_str()
        .expect("launch token should be returned");
    assert_eq!(issued["expected_harness"], "codex");
    assert_eq!(issued["expected_adapter_identity"], "codex-mcp-entry");
    assert_eq!(issued["expected_adapter_version"], "1.0.0");
    assert!(issued["expires_at"].as_i64().unwrap() > issued["created_at"].as_i64().unwrap());

    let connected = successful_json(connect_attachment(
        &daemon,
        token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-session-1",
            &full_entry_capabilities(),
        ),
        "connect-codex-session-1",
    ));
    assert_eq!(connected["attachment"]["mode"], "full");
    assert_eq!(connected["attachment"]["mode_tag"], "Tyrion: Full");
    assert_eq!(
        connected["attachment"]["native_session_id"],
        "codex-session-1"
    );
    assert!(connected["attachment_session_token"]
        .as_str()
        .is_some_and(|token| token.starts_with("tat_")));
    assert_eq!(connected["attachment"]["missing_capabilities"], json!([]));

    let exact_replay = connect_attachment(
        &daemon,
        token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-session-1",
            &full_entry_capabilities(),
        ),
        "connect-codex-session-1",
    );
    assert_attachment_rejected(&exact_replay, "already used");

    let replayed_token = connect_attachment(
        &daemon,
        token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-session-2",
            &full_entry_capabilities(),
        ),
        "replay-codex-token",
    );
    assert_eq!(replayed_token.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&replayed_token.stderr)
        .expect("attachment failure should be structured JSON");
    assert_eq!(error["code"], "attachment_rejected");
    assert!(error["message"].as_str().unwrap().contains("already used"));
}

#[test]
fn explicit_takeover_transfers_commission_control() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    write_proposal(&proposal_path, &proposal());
    let daemon = RunningDaemon::start(temp.path());

    let first_token = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-first-control-token",
    );
    let first = successful_json(connect_attachment(
        &daemon,
        first_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-control-session",
            &full_entry_capabilities(),
        ),
        "connect-first-control-session",
    ));
    let first_id = first["attachment"]["id"].as_str().unwrap().to_owned();
    let first_session_token = first["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();

    let impersonation = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &first_id])
        .args(["commission", "inspect", "unknown"])
        .output()
        .expect("CLI should run");
    assert_eq!(impersonation.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&impersonation.stderr)
        .expect("credential failure should be structured JSON");
    assert_eq!(error["code"], "control_denied");
    assert!(error["message"].as_str().unwrap().contains("invalid"));

    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &first_session_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-controlled-proposal",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    assert_eq!(created["attachments"][0]["role"], "active");

    let second_token = issue_launch_token(
        &daemon,
        "claude",
        "claude-mcp-entry",
        "1.0.0",
        "issue-observer-token",
    );
    let second = successful_json(connect_attachment_with_context(
        &daemon,
        second_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "claude",
            "claude-mcp-entry",
            "1.0.0",
            "claude-observer-session",
            &full_entry_capabilities(),
        ),
        Some(commission_id),
        "connect-observer-session",
    ));
    let second_id = second["attachment"]["id"].as_str().unwrap().to_owned();
    let second_session_token = second["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(second["commission_role"], "observer");

    let observer_acceptance = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &second_session_token])
        .args([
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "observer-cannot-accept",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(observer_acceptance.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&observer_acceptance.stderr)
        .expect("control denial should be structured JSON");
    assert_eq!(error["code"], "control_denied");

    let takeover = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_session_token,
            "commission",
            "take-control",
            commission_id,
            "--expected-revision",
            "0",
            "--expected-control-revision",
            "0",
            "--idempotency-key",
            "take-control-with-claude",
        ],
    );
    assert_eq!(takeover["commission_revision"], 0);
    assert_eq!(takeover["control_revision"], 1);
    assert_eq!(takeover["active_attachment_id"], second_id);
    let attachments = takeover["attachments"].as_array().unwrap();
    assert_eq!(
        attachments
            .iter()
            .find(|attachment| attachment["id"] == first_id)
            .unwrap()["role"],
        "observer"
    );
    assert_eq!(
        attachments
            .iter()
            .find(|attachment| attachment["id"] == second_id)
            .unwrap()["role"],
        "active"
    );

    let stale_takeover = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &first_session_token])
        .args([
            "commission",
            "take-control",
            commission_id,
            "--expected-revision",
            "0",
            "--expected-control-revision",
            "0",
            "--idempotency-key",
            "reject-stale-control-takeover",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(stale_takeover.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&stale_takeover.stderr)
        .expect("stale takeover should be structured JSON");
    assert_eq!(error["code"], "stale_control_revision");

    let handoff_events = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_session_token,
            "attachment",
            "replay",
            commission_id,
            "--after-sequence",
            "0",
        ],
    );
    let handoff = handoff_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "active_attachment_changed")
        .expect("control takeover should be a durable ordered event");
    assert_eq!(
        handoff["payload"]["previous_active_attachment_id"],
        first_id
    );
    assert_eq!(handoff["payload"]["active_attachment_id"], second_id);
    assert_eq!(handoff["payload"]["control_revision"], 1);

    daemon.stop();
    let daemon = RunningDaemon::start(temp.path());

    let former_controller = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &first_session_token])
        .args([
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "former-controller-cannot-accept",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(former_controller.status.code(), Some(2));

    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_session_token,
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "new-controller-accepts",
        ],
    );
    assert_eq!(accepted["commission"]["status"], "active");
    assert_eq!(accepted["commission"]["authority"], proposal()["authority"]);
}

#[test]
fn reconnect_replays_unseen_events_after_disconnected_completion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    write_proposal(&proposal_path, &proposal());
    let daemon = RunningDaemon::start(temp.path());

    let controller_token = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-replay-controller-token",
    );
    let controller = successful_json(connect_attachment(
        &daemon,
        controller_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-replay-session",
            &full_entry_capabilities(),
        ),
        "connect-replay-controller",
    ));
    let controller_session_token = controller["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &controller_session_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-replay-proposal",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();

    let observer_token = issue_launch_token(
        &daemon,
        "claude",
        "claude-mcp-entry",
        "1.0.0",
        "issue-replay-observer-token",
    );
    let observer = successful_json(connect_attachment_with_context(
        &daemon,
        observer_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "claude",
            "claude-mcp-entry",
            "1.0.0",
            "claude-replay-session",
            &full_entry_capabilities(),
        ),
        Some(commission_id),
        "connect-replay-observer",
    ));
    let observer_session_token = observer["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(observer["replay"]["commission_role"], "observer");
    assert!(observer["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|event| event["sequence"].as_i64().unwrap() > 0));
    assert!(observer["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["type"] == "commission_proposed"));

    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &controller_session_token,
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-while-entry-disconnects",
        ],
    );
    wait_for_event(
        &daemon,
        &controller_session_token,
        commission_id,
        "commission_verified_complete",
    );
    daemon.stop();

    let restarted = RunningDaemon::start(temp.path());
    let controller_replay = successful_json(resume_attachment(
        &restarted,
        &controller_session_token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "codex-replay-session",
            &full_entry_capabilities(),
        ),
        commission_id,
        0,
    ))["replay"]
        .clone();
    assert_eq!(controller_replay["commission_role"], "active");
    let events = controller_replay["events"].as_array().unwrap();
    assert!(events
        .windows(2)
        .all(|pair| pair[0]["sequence"].as_i64().unwrap() < pair[1]["sequence"].as_i64().unwrap()));
    assert!(events
        .iter()
        .any(|event| event["type"] == "commission_verified_complete"));
    assert_eq!(
        controller_replay["material_notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["commission_verified_complete"]
    );

    let observer_replay = successful_json(resume_attachment(
        &restarted,
        &observer_session_token,
        &attachment_fixture(
            "claude",
            "claude-mcp-entry",
            "1.0.0",
            "claude-replay-session",
            &full_entry_capabilities(),
        ),
        commission_id,
        0,
    ))["replay"]
        .clone();
    assert_eq!(observer_replay["commission_role"], "observer");
    assert_eq!(observer_replay["events"], controller_replay["events"]);
    assert_eq!(observer_replay["material_notifications"], json!([]));

    let cursor = controller_replay["next_event_sequence"]
        .as_i64()
        .unwrap()
        .to_string();
    let no_duplicates = run_cli(
        &restarted.socket_path,
        &[
            "--attachment-token",
            &controller_session_token,
            "attachment",
            "replay",
            commission_id,
            "--after-sequence",
            &cursor,
        ],
    );
    assert_eq!(no_duplicates["events"], json!([]));
    assert_eq!(
        no_duplicates["next_event_sequence"],
        controller_replay["next_event_sequence"]
    );

    let mismatched_resume = resume_attachment(
        &restarted,
        &controller_session_token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "different-native-session",
            &full_entry_capabilities(),
        ),
        commission_id,
        0,
    );
    assert_attachment_rejected(&mismatched_resume, "does not match");
}

#[test]
fn rejected_attachment_handshakes_never_fall_back_or_consume_a_valid_token() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let full = full_entry_capabilities();

    let bound = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-bound-token",
    );
    let token = bound["launch_token"].as_str().unwrap();
    let mismatch = connect_attachment(
        &daemon,
        token,
        &attachment_fixture(
            "claude",
            "codex-mcp-entry",
            "1.0.0",
            "mismatched-session",
            &full,
        ),
        "connect-mismatched-session",
    );
    assert_attachment_rejected(&mismatch, "harness identity mismatch");
    successful_json(connect_attachment(
        &daemon,
        token,
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "correct-session",
            &full,
        ),
        "connect-correct-session",
    ));

    let adapter_bound = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-adapter-bound-token",
    );
    let adapter_mismatch = connect_attachment(
        &daemon,
        adapter_bound["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "different-adapter",
            "1.0.0",
            "adapter-mismatched-session",
            &full,
        ),
        "connect-adapter-mismatched-session",
    );
    assert_attachment_rejected(&adapter_mismatch, "adapter identity mismatch");

    let invalid = connect_attachment(
        &daemon,
        "tlt_not-a-real-token",
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "invalid-token-session",
            &full,
        ),
        "connect-invalid-token",
    );
    assert_attachment_rejected(&invalid, "launch token is invalid");

    let incompatible = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-incompatible-token",
    );
    let mut incompatible_fixture = attachment_fixture(
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "incompatible-session",
        &full,
    );
    incompatible_fixture.protocol_version = 3;
    let incompatible = connect_attachment(
        &daemon,
        incompatible["launch_token"].as_str().unwrap(),
        &incompatible_fixture,
        "connect-incompatible-session",
    );
    assert_attachment_rejected(
        &incompatible,
        "adapter protocol version 3 is incompatible with 2",
    );

    let expired = issue_launch_token_with_ttl(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        1,
        "issue-expiring-token",
    );
    thread::sleep(Duration::from_millis(1_100));
    let expired = connect_attachment(
        &daemon,
        expired["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "expired-session",
            &full,
        ),
        "connect-expired-session",
    );
    assert_attachment_rejected(&expired, "launch token has expired");
}

fn assert_attachment_rejected(output: &Output, expected_message: &str) {
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "failed attachment must not launch a fallback"
    );
    let error: Value = serde_json::from_slice(&output.stderr)
        .expect("attachment failure should be structured JSON");
    assert_eq!(error["code"], "attachment_rejected");
    let actual_message = error["message"].as_str().unwrap();
    assert!(
        actual_message.contains(expected_message),
        "expected attachment error containing {expected_message:?}, got {actual_message:?}"
    );
}

#[test]
fn capability_negotiation_reports_limited_and_observer_effects() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());

    let limited_token = issue_launch_token(
        &daemon,
        "codex",
        "codex-mcp-entry",
        "1.0.0",
        "issue-limited-token",
    );
    let limited_capabilities = full_entry_capabilities()
        .into_iter()
        .filter(|capability| *capability != "material_notifications")
        .collect::<Vec<_>>();
    let limited = successful_json(connect_attachment(
        &daemon,
        limited_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "codex",
            "codex-mcp-entry",
            "1.0.0",
            "limited-session",
            &limited_capabilities,
        ),
        "connect-limited-session",
    ));
    assert_eq!(limited["attachment"]["mode"], "limited");
    assert_eq!(limited["attachment"]["mode_tag"], "Tyrion: Limited");
    assert_eq!(
        limited["attachment"]["missing_capabilities"][0]["capability"],
        "material_notifications"
    );
    assert!(limited["attachment"]["missing_capabilities"][0]["effect"]
        .as_str()
        .unwrap()
        .contains("inspect"));

    let observer_token = issue_launch_token(
        &daemon,
        "muse",
        "muse-entry",
        "0.1.0",
        "issue-observer-mode-token",
    );
    let observer = successful_json(connect_attachment(
        &daemon,
        observer_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "muse",
            "muse-entry",
            "0.1.0",
            "observer-mode-session",
            &[
                "commission_inspection",
                "event_replay",
                "persistent_mode_display",
            ],
        ),
        "connect-observer-mode-session",
    ));
    assert_eq!(observer["attachment"]["mode"], "observer");
    assert_eq!(observer["attachment"]["mode_tag"], "Tyrion: Observer");
    assert!(observer["attachment"]["missing_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|missing| missing["effect"]
            .as_str()
            .is_some_and(|effect| !effect.is_empty())
            && missing["alternative"]
                .as_str()
                .is_some_and(|alternative| !alternative.is_empty())));

    let incompatible_token = issue_launch_token(
        &daemon,
        "muse",
        "muse-entry",
        "0.1.0",
        "issue-no-inspection-token",
    );
    let incompatible = connect_attachment(
        &daemon,
        incompatible_token["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "muse",
            "muse-entry",
            "0.1.0",
            "no-inspection-session",
            &["event_replay"],
        ),
        "connect-no-inspection-session",
    );
    assert_attachment_rejected(&incompatible, "commission_inspection is required");
}

#[test]
fn model_verification_requirement_remains_visibly_uncertain() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([{
        "id": "maintainability",
        "description": "The Result is maintainable",
        "required_evidence": "maintainability_review",
        "verifier_type": "model",
        "verification_depth": "independent",
        "verifier_configuration": "review-model-v1",
        "verification_environment": "contained-review-v1",
        "verifier": {
            "kind": "prompt",
            "prompt": "Judge whether the Result is maintainable."
        }
    }]);
    review_proposal["resource_ceilings"]["max_attempts"] = json!(2);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "model-verification");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-model-verification",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-model-verification",
        ],
    );

    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let inspected = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );

    assert_eq!(inspected["commission"]["status"], "active");
    assert_eq!(
        inspected["criteria"][0]["required_evidence"],
        "maintainability_review"
    );
    assert_eq!(inspected["criteria"][0]["verifier_type"], "model");
    assert_eq!(
        inspected["criteria"][0]["verification_depth"],
        "independent"
    );
    assert_eq!(inspected["criteria"][0]["status"], "uncertain");
    assert_eq!(
        inspected["assignments"][0]["status"],
        "verification_pending"
    );
    assert_eq!(inspected["results"][0]["status"], "candidate");
    assert_eq!(inspected["verification"]["verdict"], "uncertain");
    assert_eq!(inspected["verification"]["next_action"], "retry");
    assert_eq!(inspected["briefing"], Value::Null);
}

#[test]
fn independent_deterministic_depth_uses_distinct_verification_attempts() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let mut independent = proposal();
    independent["criteria"][0]["required_evidence"] = json!("exact_output");
    independent["criteria"][0]["verification_depth"] = json!("independent");
    write_proposal(&proposal_path, &independent);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "independent-deterministic");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-independent-deterministic",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-independent-deterministic",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "commission_verified_complete",
    );
    let completed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    let current = completed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["current"] == true)
        .collect::<Vec<_>>();

    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(current.len(), 2);
    assert_ne!(
        current[0]["verification_attempt_id"],
        current[1]["verification_attempt_id"]
    );
    assert_ne!(
        current[0]["verifier_identity"],
        current[1]["verifier_identity"]
    );
}

#[test]
fn independent_model_and_principal_evidence_gate_atomic_completion() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let evidence_path = temp.path().join("evidence.json");
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([
        {
            "id": "maintainability",
            "description": "The Result is maintainable",
            "required_evidence": "maintainability_review",
            "verifier_type": "model",
            "verification_depth": "independent",
            "verifier_configuration": "review-model-v1",
            "verification_environment": "contained-review-v1",
            "verifier": {
                "kind": "prompt",
                "prompt": "Judge whether the Result is maintainable."
            }
        },
        {
            "id": "principal-intent",
            "description": "The Result matches Principal intent",
            "required_evidence": "principal_judgment",
            "verifier_type": "principal",
            "verification_depth": "standard",
            "verifier_configuration": "principal-v1",
            "verification_environment": "active-attachment-v1",
            "verifier": {
                "kind": "prompt",
                "prompt": "Confirm that the Result matches the accepted intent."
            }
        }
    ]);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "independent-verification");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-independent-verification",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-independent-verification",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let pending = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    let result_id = pending["results"][0]["id"].as_str().unwrap();
    let producer_attempt_id = pending["attempts"][0]["id"].as_str().unwrap();
    assert_eq!(pending["verification_gates"][0]["status"], "open");
    assert_eq!(pending["verification_gates"][0]["current"], true);

    let model_evidence = || {
        json!({
            "criterion_id": "maintainability",
            "result_id": result_id,
            "evidence_type": "maintainability_review",
            "verdict": "passed",
            "verifier_configuration": "review-model-v1",
            "procedure": {
                "kind": "prompt",
                "prompt": "Judge whether the Result is maintainable."
            },
            "environment": "contained-review-v1",
            "inspectable_output": "The change is focused and preserves the public seam.",
            "material_contradiction": false
        })
    };

    let mut spoofed_evidence = model_evidence();
    spoofed_evidence["verifier_identity"] = json!("producer");
    spoofed_evidence["verification_attempt_id"] = json!(producer_attempt_id);
    write_proposal(&evidence_path, &spoofed_evidence);
    let spoofed_review = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &attachment_token])
        .args([
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "reject-spoofed-verifier-identity",
        ])
        .output()
        .expect("CLI should run");
    assert_eq!(spoofed_review.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&spoofed_review.stderr).contains("unknown field"));

    write_proposal(&evidence_path, &model_evidence());
    let bound_review = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-self-review",
        ],
    );
    assert_eq!(bound_review["commission"]["status"], "active");
    assert_ne!(
        bound_review["evidence"][0]["verification_attempt_id"],
        producer_attempt_id
    );
    assert_ne!(bound_review["evidence"][0]["verifier_identity"], "producer");
    assert!(bound_review["evidence"][0]["verifier_identity"]
        .as_str()
        .unwrap()
        .starts_with("attachment:"));

    let second_launch = issue_launch_token(
        &daemon,
        "claude",
        "claude-mcp-entry",
        "1.0.0",
        "issue-independent-reviewer-token",
    );
    let second = successful_json(connect_attachment_with_context(
        &daemon,
        second_launch["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "claude",
            "claude-mcp-entry",
            "1.0.0",
            "independent-reviewer-session",
            &full_entry_capabilities(),
        ),
        Some(&commission_id),
        "connect-independent-reviewer",
    ));
    let second_attachment_token = second["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "take-control",
            &commission_id,
            "--expected-revision",
            "1",
            "--expected-control-revision",
            "0",
            "--idempotency-key",
            "independent-reviewer-takes-control",
        ],
    );

    write_proposal(&evidence_path, &model_evidence());
    let recorded = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-review-b",
        ],
    );
    assert_eq!(recorded["commission"]["status"], "active");
    assert_ne!(
        recorded["evidence"][0]["verifier_identity"],
        recorded["evidence"][1]["verifier_identity"]
    );

    let model_passed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(model_passed["criteria"][0]["status"], "passed");
    assert_eq!(model_passed["criteria"][1]["status"], "uncertain");
    assert_eq!(model_passed["verification"]["next_action"], "escalate");
    assert_eq!(model_passed["verification_gates"][0]["status"], "open");

    write_proposal(
        &evidence_path,
        &json!({
            "criterion_id": "principal-intent",
            "result_id": result_id,
            "evidence_type": "principal_judgment",
            "verdict": "passed",
            "verifier_configuration": "principal-v1",
            "procedure": {
                "kind": "prompt",
                "prompt": "Confirm that the Result matches the accepted intent."
            },
            "environment": "active-attachment-v1",
            "inspectable_output": "The Result matches the accepted intent.",
            "material_contradiction": false
        }),
    );
    let completed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-principal-review",
        ],
    );

    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["commission"]["revision"], 2);
    assert_eq!(completed["assignments"][0]["status"], "accepted");
    assert_eq!(completed["results"][0]["status"], "accepted");
    assert_eq!(completed["verification"]["verdict"], "passed");
    assert_eq!(completed["verification"]["next_action"], "closed");
    assert_eq!(completed["verification_gates"][0]["status"], "closed");
    assert_eq!(completed["verification_gates"][0]["current"], true);
    assert_eq!(completed["briefing"]["title"], "Verified Completion");
}

#[test]
fn result_defect_routes_rework_and_stales_prior_evidence() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let evidence_path = temp.path().join("evidence.json");
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([{
        "id": "quality",
        "description": "The Result meets the accepted quality bar",
        "required_evidence": "quality_review",
        "verifier_type": "model",
        "verification_depth": "standard",
        "verifier_configuration": "review-model-v1",
        "verification_environment": "contained-review-v1",
        "verifier": {
            "kind": "prompt",
            "prompt": "Judge whether the Result meets the accepted quality bar."
        }
    }]);
    review_proposal["resource_ceilings"]["max_attempts"] = json!(2);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start_with_incorrect_first_result(temp.path());
    let attachment_token = connect_full_entry(&daemon, "result-rework");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-result-rework",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-result-rework",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let first = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    let first_result_id = first["results"][0]["id"].as_str().unwrap();
    let first_artifact_revision = first["commission"]["artifact_revision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first["results"][0]["output"], "insufficient worker result");

    let review =
        |result_id: &str, identity: &str, _attempt: &str, verdict: &str, defect: Option<&str>| {
            let mut evidence = json!({
                "criterion_id": "quality",
                "result_id": result_id,
                "evidence_type": "quality_review",
                "verdict": verdict,
                "verifier_configuration": "review-model-v1",
                "procedure": {
                    "kind": "prompt",
                    "prompt": "Judge whether the Result meets the accepted quality bar."
                },
                "environment": "contained-review-v1",
                "inspectable_output": format!("{identity} returned {verdict}."),
                "material_contradiction": false
            });
            if let Some(defect) = defect {
                evidence["defect"] = json!(defect);
            }
            evidence
        };
    for (evidence, key) in [
        (
            review(
                first_result_id,
                "reviewer-a",
                "first-review-a",
                "uncertain",
                Some("environment"),
            ),
            "record-first-review-a",
        ),
        (
            review(
                first_result_id,
                "reviewer-b",
                "first-review-b",
                "failed",
                Some("result"),
            ),
            "record-first-review-b",
        ),
    ] {
        write_proposal(&evidence_path, &evidence);
        run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "commission",
                "record-evidence",
                &commission_id,
                "--file",
                path_text(&evidence_path),
                "--expected-revision",
                "1",
                "--idempotency-key",
                key,
            ],
        );
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let reworked = loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "commission",
                "inspect",
                &commission_id,
            ],
        );
        if inspected["results"].as_array().unwrap().len() == 2
            && inspected["assignments"][0]["status"] == "verification_pending"
        {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "rework did not finish before the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(reworked["attempts"].as_array().unwrap().len(), 2);
    let results = reworked["results"].as_array().unwrap();
    let superseded = results
        .iter()
        .find(|result| result["id"] == first_result_id)
        .unwrap();
    let current_result = results
        .iter()
        .find(|result| result["status"] == "candidate")
        .unwrap();
    assert_eq!(superseded["status"], "superseded");
    assert_eq!(current_result["output"], proposal()["goal"]);
    assert_ne!(
        reworked["commission"]["artifact_revision"],
        first_artifact_revision
    );
    assert!(reworked["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|evidence| evidence["current"] == false));
    let rework = reworked["verification_recoveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recovery| recovery["action"] == "rework")
        .unwrap();
    assert_eq!(rework["status"], "resolved");

    let second_result_id = current_result["id"].as_str().unwrap();
    write_proposal(
        &evidence_path,
        &review(
            second_result_id,
            "reviewer-a",
            "second-review-a",
            "passed",
            None,
        ),
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-second-review-a",
        ],
    );
    let completed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(
        completed["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|result| result["id"] == second_result_id)
            .unwrap()["status"],
        "accepted"
    );
}

#[test]
fn verification_defects_derive_retry_reroute_escalation_and_blocker() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let evidence_path = temp.path().join("evidence.json");
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([{
        "id": "quality",
        "description": "The Result meets the accepted quality bar",
        "required_evidence": "quality_review",
        "verifier_type": "model",
        "verification_depth": "standard",
        "verifier_configuration": "review-model-v1",
        "verification_environment": "contained-review-v1",
        "verifier": {
            "kind": "prompt",
            "prompt": "Judge whether the Result meets the accepted quality bar."
        }
    }]);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "verification-diagnosis");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-verification-diagnosis",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-verification-diagnosis",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let pending = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    let result_id = pending["results"][0]["id"].as_str().unwrap();

    for (verdict, defect, attempt, expected_action, expected_status) in [
        (
            "uncertain",
            "environment",
            "diagnosis-1",
            "retry",
            "pending",
        ),
        ("failed", "verifier", "diagnosis-2", "reroute", "pending"),
        (
            "uncertain",
            "criterion",
            "diagnosis-3",
            "escalate",
            "attention_required",
        ),
        ("failed", "result", "diagnosis-4", "block", "blocked"),
    ] {
        write_proposal(
            &evidence_path,
            &json!({
                "criterion_id": "quality",
                "result_id": result_id,
                "evidence_type": "quality_review",
                "verdict": verdict,
                "verifier_configuration": "review-model-v1",
                "procedure": {
                    "kind": "prompt",
                    "prompt": "Judge whether the Result meets the accepted quality bar."
                },
                "environment": "contained-review-v1",
                "inspectable_output": format!("The check diagnosed a {defect} defect."),
                "material_contradiction": false,
                "defect": defect
            }),
        );
        let recorded = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "commission",
                "record-evidence",
                &commission_id,
                "--file",
                path_text(&evidence_path),
                "--expected-revision",
                "1",
                "--idempotency-key",
                attempt,
            ],
        );
        assert_eq!(recorded["verification"]["next_action"], expected_action);
        let recovery = recorded["verification_recoveries"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(recovery["action"], expected_action);
        assert_eq!(recovery["status"], expected_status);
        assert_eq!(recovery["current"], true);
        assert!(recovery["requirement"].as_str().unwrap().len() > 20);
    }

    let blocked = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(blocked["commission"]["status"], "active");
    assert_eq!(blocked["assignments"][0]["status"], "resource_blocked");
    assert_eq!(blocked["blockers"][0]["code"], "max_attempts");
    assert_eq!(blocked["verification"]["verdict"], "uncertain");
    assert_eq!(blocked["verification"]["next_action"], "block");
}

#[test]
fn material_contradiction_prevents_completion_and_is_visible() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let evidence_path = temp.path().join("evidence.json");
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([{
        "id": "quality",
        "description": "The Result meets the accepted quality bar",
        "required_evidence": "quality_review",
        "verifier_type": "model",
        "verification_depth": "standard",
        "verifier_configuration": "review-model-v1",
        "verification_environment": "contained-review-v1",
        "verifier": {
            "kind": "prompt",
            "prompt": "Judge whether the Result meets the accepted quality bar."
        }
    }]);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "material-contradiction");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-material-contradiction",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-material-contradiction",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let pending = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    write_proposal(
        &evidence_path,
        &json!({
            "criterion_id": "quality",
            "result_id": pending["results"][0]["id"],
            "evidence_type": "quality_review",
            "verdict": "passed",
            "verifier_configuration": "review-model-v1",
            "procedure": {
                "kind": "prompt",
                "prompt": "Judge whether the Result meets the accepted quality bar."
            },
            "environment": "contained-review-v1",
            "inspectable_output": "The check passed, but conflicts with material repository Evidence.",
            "material_contradiction": true
        }),
    );
    let contradicted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-material-contradiction",
        ],
    );

    assert_eq!(contradicted["commission"]["status"], "active");
    assert_eq!(contradicted["criteria"][0]["status"], "uncertain");
    assert_eq!(contradicted["verification"]["verdict"], "uncertain");
    assert_eq!(contradicted["verification"]["next_action"], "escalate");
    assert_eq!(contradicted["evidence"][0]["material_contradiction"], true);
    assert_eq!(contradicted["briefing"], Value::Null);
}

#[test]
fn verification_amendment_retains_and_stales_prior_mandate_evidence() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let proposal_path = temp.path().join("proposal.json");
    let amendment_path = temp.path().join("amendment.json");
    let evidence_path = temp.path().join("evidence.json");
    let criterion = |configuration: &str, prompt: &str| {
        json!({
            "id": "quality",
            "description": "The Result meets the accepted quality bar",
            "required_evidence": "quality_review",
            "verifier_type": "model",
            "verification_depth": "independent",
            "verifier_configuration": configuration,
            "verification_environment": "contained-review-v1",
            "verifier": {"kind": "prompt", "prompt": prompt}
        })
    };
    let mut review_proposal = proposal();
    review_proposal["criteria"] = json!([criterion(
        "review-model-v1",
        "Judge the Result against quality bar v1."
    )]);
    review_proposal["resource_ceilings"]["max_attempts"] = json!(2);
    write_proposal(&proposal_path, &review_proposal);

    let daemon = RunningDaemon::start(temp.path());
    let attachment_token = connect_full_entry(&daemon, "verification-amendment");
    let commission_id = create_proposal(
        &daemon,
        &proposal_path,
        &attachment_token,
        "proposal-create-verification-amendment",
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            &commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "commission-accept-verification-amendment",
        ],
    );
    wait_for_event(
        &daemon,
        &attachment_token,
        &commission_id,
        "result_integrated",
    );
    let first = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    let first_result_id = first["results"][0]["id"].as_str().unwrap();
    write_proposal(
        &evidence_path,
        &json!({
            "criterion_id": "quality",
            "result_id": first_result_id,
            "evidence_type": "quality_review",
            "verdict": "passed",
            "verifier_configuration": "review-model-v1",
            "procedure": {
                "kind": "prompt",
                "prompt": "Judge the Result against quality bar v1."
            },
            "environment": "contained-review-v1",
            "inspectable_output": "The Result passes quality bar v1.",
            "material_contradiction": false
        }),
    );
    let first_review = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "record-mandate-1-review-a",
        ],
    );
    assert_eq!(first_review["evidence"][0]["current"], true);

    write_proposal(
        &amendment_path,
        &json!({
            "criteria": [criterion(
                "review-model-v2",
                "Judge the Result against the clarified quality bar v2."
            )]
        }),
    );
    let amended = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "amend-verification",
            &commission_id,
            "--file",
            path_text(&amendment_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "amend-verification-v2",
        ],
    );
    assert_eq!(amended["commission"]["revision"], 2);
    assert_eq!(amended["assignments"][0]["status"], "ready");
    assert_eq!(amended["criteria"][0]["status"], "uncertain");
    assert_eq!(
        amended["criteria"][0]["verifier_configuration"],
        "review-model-v2"
    );
    assert_eq!(amended["criterion_versions"].as_array().unwrap().len(), 2);
    assert_eq!(amended["evidence"][0]["mandate_revision"], 1);
    assert_eq!(amended["evidence"][0]["current"], false);

    let deadline = Instant::now() + Duration::from_secs(5);
    let reverified = loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "commission",
                "inspect",
                &commission_id,
            ],
        );
        if inspected["results"].as_array().unwrap().len() == 2
            && inspected["assignments"][0]["status"] == "verification_pending"
        {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "amended verification did not finish before the deadline"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let current_result_id = reverified["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["status"] == "candidate")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let final_evidence = json!({
        "criterion_id": "quality",
        "result_id": current_result_id,
        "evidence_type": "quality_review",
        "verdict": "passed",
        "verifier_configuration": "review-model-v2",
        "procedure": {
            "kind": "prompt",
            "prompt": "Judge the Result against the clarified quality bar v2."
        },
        "environment": "contained-review-v1",
        "inspectable_output": "The Result passes quality bar v2.",
        "material_contradiction": false
    });
    write_proposal(&evidence_path, &final_evidence);
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "2",
            "--idempotency-key",
            "record-mandate-2-review-a",
        ],
    );

    let second_launch = issue_launch_token(
        &daemon,
        "claude",
        "claude-mcp-entry",
        "1.0.0",
        "issue-amendment-reviewer-token",
    );
    let second = successful_json(connect_attachment_with_context(
        &daemon,
        second_launch["launch_token"].as_str().unwrap(),
        &attachment_fixture(
            "claude",
            "claude-mcp-entry",
            "1.0.0",
            "amendment-reviewer-session",
            &full_entry_capabilities(),
        ),
        Some(&commission_id),
        "connect-amendment-reviewer",
    ));
    let second_attachment_token = second["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned();
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "take-control",
            &commission_id,
            "--expected-revision",
            "2",
            "--expected-control-revision",
            "0",
            "--idempotency-key",
            "amendment-reviewer-takes-control",
        ],
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &second_attachment_token,
            "commission",
            "record-evidence",
            &commission_id,
            "--file",
            path_text(&evidence_path),
            "--expected-revision",
            "2",
            "--idempotency-key",
            "record-mandate-2-review-b",
        ],
    );
    let completed = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["commission"]["revision"], 3);
    assert_eq!(
        completed["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["current"] == true)
            .count(),
        2
    );
}

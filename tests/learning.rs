#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

struct RunningDaemon {
    child: Child,
    data_dir: PathBuf,
    socket_path: PathBuf,
    principal_token: String,
}

impl RunningDaemon {
    fn start(data_dir: &Path) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let (child, principal_token) = Self::spawn(data_dir, &socket_path, true);
        let mut daemon = Self {
            child,
            data_dir: data_dir.to_owned(),
            socket_path,
            principal_token,
        };
        daemon.wait_until_ready();
        daemon
    }

    fn spawn(data_dir: &Path, socket_path: &Path, defer_dispatch: bool) -> (Child, String) {
        let mut descriptors = [0_i32; 2];
        // SAFETY: pipe initializes both descriptors, which are closed exactly once below.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let bootstrap_fd = descriptors[1].to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command.args([
            "--data-dir",
            path_text(data_dir),
            "--socket",
            path_text(socket_path),
            "--principal-control-bootstrap-fd",
            &bootstrap_fd,
        ]);
        if defer_dispatch {
            command.arg("--fault-defer-ready-dispatch");
        }
        let bootstrap_read_fd = descriptors[0];
        // SAFETY: close is async-signal-safe and removes the launcher's read end from the child.
        unsafe {
            command.pre_exec(move || {
                if libc::close(bootstrap_read_fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let child = command.spawn().expect("daemon should start");
        // SAFETY: the child inherited the write descriptor and the parent no longer needs it.
        assert_eq!(unsafe { libc::close(descriptors[1]) }, 0);
        // SAFETY: the parent owns the initialized read descriptor until File drops it.
        let bootstrap_pipe = unsafe { fs::File::from_raw_fd(descriptors[0]) };
        let mut bootstrap = String::new();
        BufReader::new(bootstrap_pipe)
            .read_line(&mut bootstrap)
            .expect("Principal bootstrap credential should be readable");
        let principal_token = bootstrap
            .trim()
            .strip_prefix("TYRION_PRINCIPAL_CONTROL_TOKEN=")
            .expect("daemon should emit the Principal bootstrap credential")
            .to_owned();
        (child, principal_token)
    }

    fn restart(&mut self) {
        self.child.kill().expect("daemon should stop");
        self.child.wait().expect("daemon should be reaped");
        let (child, principal_token) = Self::spawn(&self.data_dir, &self.socket_path, true);
        self.child = child;
        self.principal_token = principal_token;
        self.wait_until_ready();
    }

    fn restart_with_dispatch(&mut self) {
        self.child.kill().expect("daemon should stop");
        self.child.wait().expect("daemon should be reaped");
        let (child, principal_token) = Self::spawn(&self.data_dir, &self.socket_path, false);
        self.child = child;
        self.principal_token = principal_token;
        self.wait_until_ready();
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
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn principal_creates_one_durable_atomic_project_preference() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-source");
    let proposal_path = temp.path().join("source-proposal.json");
    write_proposal(
        &proposal_path,
        "project-tyrion",
        &["Use a compact response for this Commission only."],
    );
    let created = run_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-learning-source",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();

    let remembered = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            commission_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "remember-behavior-first-tests",
        ],
    );
    let claim = &remembered["claim"];
    assert_eq!(claim["version"], 1);
    assert_eq!(
        claim["statement"],
        "Prefer behavior-first tests at public seams."
    );
    assert_eq!(claim["strength"], "hard");
    assert_eq!(
        claim["scope"],
        json!({"kind": "project", "project_id": "project-tyrion"})
    );
    assert_eq!(
        claim["applicability"],
        json!({"work_kind": "software_building"})
    );
    assert_eq!(claim["provenance"]["kind"], "explicit_principal_statement");
    assert_eq!(claim["provenance"]["commission_id"], commission_id);
    assert!(claim["provenance"]["attachment_id"].is_string());
    assert_eq!(
        claim["confidence"],
        json!({"category": "explicit", "basis_points": 10_000})
    );
    assert_eq!(claim["lifecycle"], json!({"state": "active"}));
    assert!(claim["created_at"].as_i64().unwrap() > 0);
    assert_eq!(claim["updated_at"], claim["created_at"]);
    assert_eq!(
        remembered["learning_receipt"],
        json!({
            "kind": "profile_claim_created",
            "claim_id": claim["id"],
            "claim_version": 1,
            "scope": {"kind": "project", "project_id": "project-tyrion"},
        })
    );

    let claim_id = claim["id"].as_str().unwrap().to_owned();
    daemon.restart();
    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    assert_eq!(inspected["claim"], *claim);
    assert_eq!(inspected["affected_attempts"], json!([]));

    let profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-tyrion",
        ],
    );
    assert_eq!(profile["claims"], json!([claim]));
}

#[test]
fn later_assignment_receives_bounded_advisory_preference_and_records_outcome() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-loop");

    let source_path = temp.path().join("source-proposal.json");
    write_proposal_with_goal(
        &source_path,
        "project-tyrion",
        "return the source greeting",
        &["Use a compact response for this Commission only."],
    );
    let source = create_proposal(
        &daemon,
        &attachment_token,
        &source_path,
        "create-learning-loop-source",
    );
    let source_id = source["commission"]["id"].as_str().unwrap();
    let remembered = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "remember-loop-preference",
        ],
    );
    let claim_id = remembered["claim"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        source_id,
        "accept-learning-source",
    );

    let later_path = temp.path().join("later-proposal.json");
    write_proposal_with_goal(
        &later_path,
        "project-tyrion",
        "return the later greeting",
        &["Return the exact accepted greeting."],
    );
    let later = create_proposal(
        &daemon,
        &attachment_token,
        &later_path,
        "create-later-commission",
    );
    let later_id = later["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        later_id,
        "accept-later-commission",
    );
    let unrelated_path = temp.path().join("unrelated-proposal.json");
    write_proposal_with_goal(
        &unrelated_path,
        "project-unrelated",
        "return the unrelated greeting",
        &[],
    );
    let unrelated = create_proposal(
        &daemon,
        &attachment_token,
        &unrelated_path,
        "create-unrelated-commission",
    );
    let unrelated_id = unrelated["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        unrelated_id,
        "accept-unrelated-commission",
    );

    daemon.restart_with_dispatch();
    let source_completed =
        wait_for_status(&daemon, &attachment_token, source_id, "verified_complete");
    let later_completed =
        wait_for_status(&daemon, &attachment_token, later_id, "verified_complete");
    let unrelated_completed = wait_for_status(
        &daemon,
        &attachment_token,
        unrelated_id,
        "verified_complete",
    );

    assert_eq!(
        source_completed["briefing"]["learning_receipts"],
        json!([{
            "kind": "profile_claim_created",
            "claim_id": claim_id,
            "claim_version": 1,
            "scope": {"kind": "project", "project_id": "project-tyrion"},
        }])
    );
    assert_eq!(
        source_completed["attempts"][0]["worker_context_packet"]["advisory"]["profile_claims"],
        json!([]),
        "a source Commission must not retrieve a claim created from itself"
    );

    let packet = &later_completed["attempts"][0]["worker_context_packet"];
    assert_eq!(packet["version"], 1);
    assert_eq!(
        packet["precedence"],
        json!([
            "current_principal_instructions",
            "commission_constraints",
            "acceptance_criteria",
            "authority_envelope",
            "resource_ceilings",
            "current_repository_evidence",
            "advisory_profile_claims"
        ])
    );
    assert_eq!(
        packet["binding"]["current_principal_instructions"],
        json!(["return the later greeting"])
    );
    assert_eq!(
        packet["binding"]["commission_constraints"],
        json!(["Return the exact accepted greeting."])
    );
    assert_eq!(
        packet["binding"]["authority_envelope"],
        later_completed["commission"]["authority"]
    );
    assert_eq!(
        packet["binding"]["resource_ceilings"],
        later_completed["commission"]["resource_ceilings"]
    );
    assert_eq!(
        packet["binding"]["current_repository_evidence"]["project_id"],
        "project-tyrion"
    );
    let advisory_claims = packet["advisory"]["profile_claims"].as_array().unwrap();
    assert_eq!(advisory_claims.len(), 1);
    assert_eq!(advisory_claims[0]["id"], claim_id);
    assert_eq!(advisory_claims[0]["version"], 1);
    assert_eq!(advisory_claims[0]["strength"], "hard");
    assert_eq!(advisory_claims[0]["advisory"], true);
    assert_eq!(
        packet["advisory"]["authority_effect"],
        json!({
            "routing": false,
            "approval_gates": false,
            "credentials": false,
            "resource_ceilings": false,
        })
    );
    let budget = &packet["advisory"]["budget"];
    assert_eq!(budget["target_tokens"], 2_000);
    assert!(budget["tokens_used"].as_u64().unwrap() <= budget["hard_max_tokens"].as_u64().unwrap());
    assert_eq!(later_completed["credential_grants"], json!([]));
    assert_eq!(later_completed["approval_gates"], json!([]));
    assert_eq!(
        later_completed["assignments"][0]["route"]["selected_configuration"]["id"],
        "deterministic-local-v1"
    );
    assert_eq!(
        unrelated_completed["attempts"][0]["worker_context_packet"]["advisory"]["profile_claims"],
        json!([]),
        "a project-scoped claim must not reach an unrelated project"
    );

    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    let affected = inspected["affected_attempts"].as_array().unwrap();
    assert_eq!(affected.len(), 1);
    assert_eq!(affected[0]["commission_id"], later_id);
    assert_eq!(affected[0]["claim_version"], 1);
    assert_eq!(affected[0]["outcome"], "accepted");
    assert!(affected[0]["result_id"].is_string());
    assert!(affected[0]["recorded_at"].as_i64().unwrap() > 0);

    let profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-tyrion",
        ],
    );
    assert_eq!(profile["claims"].as_array().unwrap().len(), 1);
    let unrelated_profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-unrelated",
        ],
    );
    assert_eq!(unrelated_profile["claims"], json!([]));
}

#[test]
fn unsuccessful_preference_application_is_retained_in_completion_receipt() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-rework");

    let source_path = temp.path().join("rework-source.json");
    write_proposal(&source_path, "project-tyrion", &[]);
    let source = create_proposal(
        &daemon,
        &attachment_token,
        &source_path,
        "create-rework-source",
    );
    let source_id = source["commission"]["id"].as_str().unwrap();
    let remembered = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "remember-rework-preference",
        ],
    );
    let claim_id = remembered["claim"]["id"].as_str().unwrap().to_owned();

    let later_path = temp.path().join("rework-later.json");
    write_model_proposal(&later_path, "project-tyrion");
    let later = create_proposal(
        &daemon,
        &attachment_token,
        &later_path,
        "create-rework-later",
    );
    let later_id = later["commission"]["id"].as_str().unwrap();
    accept_commission(&daemon, &attachment_token, later_id, "accept-rework-later");

    daemon.restart_with_dispatch();
    let first = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        later_id,
        "verification_pending",
        1,
    );
    let first_result = first["results"][0]["id"].as_str().unwrap();
    let evidence_path = temp.path().join("evidence.json");
    write_model_evidence(&evidence_path, first_result, "failed", Some("result"));
    record_evidence(
        &daemon,
        &attachment_token,
        later_id,
        &evidence_path,
        "record-rework-failure",
    );

    let second = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        later_id,
        "verification_pending",
        2,
    );
    let second_result = second["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["status"] == "candidate")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    write_model_evidence(&evidence_path, second_result, "passed", None);
    record_evidence(
        &daemon,
        &attachment_token,
        later_id,
        &evidence_path,
        "record-rework-pass",
    );
    let completed = wait_for_status(&daemon, &attachment_token, later_id, "verified_complete");
    assert_eq!(
        completed["briefing"]["learning_receipts"],
        json!([{
            "kind": "profile_claim_applied_unsuccessfully",
            "claim_id": claim_id,
            "claim_version": 1,
            "attempt_id": completed["attempts"][0]["id"],
            "result_id": first_result,
            "outcome": "edited",
        }])
    );

    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    let outcomes = inspected["affected_attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attempt| attempt["outcome"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes, ["edited", "accepted"]);
}

fn write_proposal(path: &Path, project_id: &str, constraints: &[&str]) {
    write_proposal_with_goal(
        path,
        project_id,
        "return a deterministic greeting",
        constraints,
    );
}

fn write_proposal_with_goal(path: &Path, project_id: &str, goal: &str, constraints: &[&str]) {
    let proposal = json!({
        "project_id": project_id,
        "goal": goal,
        "commission_constraints": constraints,
        "criteria": [{
            "id": "greeting",
            "description": "The Result contains the accepted greeting",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": goal}
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
            "max_storage_bytes": 1_048_576,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_model_proposal(path: &Path, project_id: &str) {
    let proposal = json!({
        "project_id": project_id,
        "goal": "return a model-reviewed greeting",
        "criteria": [{
            "id": "greeting",
            "description": "The Result satisfies the model review",
            "required_evidence": "model_review",
            "verifier_type": "model",
            "verification_depth": "standard",
            "verifier_configuration": "review-model-v1",
            "verification_environment": "tyrion-controlled-v1",
            "verifier": {"kind": "prompt", "prompt": "Review the exact greeting."}
        }],
        "authority": {
            "repositories": [],
            "paths": [],
            "actions": ["deterministic.echo"],
            "destinations": [],
            "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 2,
            "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1,
            "max_storage_bytes": 1_048_576,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_model_evidence(path: &Path, result_id: &str, verdict: &str, defect: Option<&str>) {
    let mut evidence = json!({
        "criterion_id": "greeting",
        "result_id": result_id,
        "evidence_type": "model_review",
        "verdict": verdict,
        "verifier_configuration": "review-model-v1",
        "procedure": {"kind": "prompt", "prompt": "Review the exact greeting."},
        "environment": "tyrion-controlled-v1",
        "inspectable_output": format!("Model review verdict: {verdict}"),
        "material_contradiction": false
    });
    if let Some(defect) = defect {
        evidence["defect"] = json!(defect);
    }
    fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
}

fn create_proposal(
    daemon: &RunningDaemon,
    attachment_token: &str,
    proposal_path: &Path,
    idempotency_key: &str,
) -> Value {
    run_cli(
        daemon,
        Some(attachment_token),
        &[
            "proposal",
            "create",
            "--file",
            path_text(proposal_path),
            "--idempotency-key",
            idempotency_key,
        ],
    )
}

fn accept_commission(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    idempotency_key: &str,
) -> Value {
    run_cli(
        daemon,
        Some(attachment_token),
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            idempotency_key,
        ],
    )
}

fn wait_for_status(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    status: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inspected = run_cli(
            daemon,
            Some(attachment_token),
            &["commission", "inspect", commission_id],
        );
        if inspected["commission"]["status"] == status {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Commission {commission_id} did not reach {status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_assignment_status(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    assignment_status: &str,
    result_count: usize,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inspected = run_cli(
            daemon,
            Some(attachment_token),
            &["commission", "inspect", commission_id],
        );
        if inspected["assignments"][0]["status"] == assignment_status
            && inspected["results"].as_array().unwrap().len() == result_count
        {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Commission {commission_id} did not reach Assignment status {assignment_status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn record_evidence(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    evidence_path: &Path,
    idempotency_key: &str,
) -> Value {
    run_cli(
        daemon,
        Some(attachment_token),
        &[
            "commission",
            "record-evidence",
            commission_id,
            "--file",
            path_text(evidence_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            idempotency_key,
        ],
    )
}

fn connect_full_entry(daemon: &RunningDaemon, label: &str) -> String {
    let issued = run_cli(
        daemon,
        None,
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
            &format!("issue-{label}-token"),
        ],
    );
    let connected = run_cli(
        daemon,
        None,
        &[
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
            &format!("{label}-session"),
            "--capability",
            "proposal_creation",
            "--capability",
            "commission_acceptance",
            "--capability",
            "commission_inspection",
            "--capability",
            "event_replay",
            "--capability",
            "control_takeover",
            "--capability",
            "material_notifications",
            "--capability",
            "persistent_mode_display",
            "--capability",
            "worker_steering",
            "--capability",
            "worker_interruption",
            "--idempotency-key",
            &format!("connect-{label}-session"),
        ],
    );
    connected["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn run_cli(daemon: &RunningDaemon, attachment_token: Option<&str>, arguments: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tyrion"));
    command.args(["--socket", path_text(&daemon.socket_path)]);
    if let Some(attachment_token) = attachment_token {
        command.args(["--attachment-token", attachment_token]);
    }
    command.args(arguments);
    successful_json(command.output().expect("CLI should run"))
}

fn run_principal_cli(
    daemon: &RunningDaemon,
    attachment_token: Option<&str>,
    arguments: &[&str],
) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tyrion"));
    command
        .args(["--socket", path_text(&daemon.socket_path)])
        .arg("--principal-token-stdin");
    if let Some(attachment_token) = attachment_token {
        command.args(["--attachment-token", attachment_token]);
    }
    let mut child = command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Principal CLI should run");
    writeln!(child.stdin.as_mut().unwrap(), "{}", daemon.principal_token).unwrap();
    successful_json(child.wait_with_output().unwrap())
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

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

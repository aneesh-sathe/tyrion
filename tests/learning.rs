#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
        let (child, principal_token) = Self::spawn(data_dir, &socket_path, true, None);
        let mut daemon = Self {
            child,
            data_dir: data_dir.to_owned(),
            socket_path,
            principal_token,
        };
        daemon.wait_until_ready();
        daemon
    }

    fn spawn(
        data_dir: &Path,
        socket_path: &Path,
        defer_dispatch: bool,
        memory_now_epoch: Option<i64>,
    ) -> (Child, String) {
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
        if let Some(memory_now_epoch) = memory_now_epoch {
            command.args(["--fault-memory-now-epoch", &memory_now_epoch.to_string()]);
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
        let (child, principal_token) = Self::spawn(&self.data_dir, &self.socket_path, true, None);
        self.child = child;
        self.principal_token = principal_token;
        self.wait_until_ready();
    }

    fn restart_with_dispatch(&mut self) {
        self.child.kill().expect("daemon should stop");
        self.child.wait().expect("daemon should be reaped");
        let (child, principal_token) = Self::spawn(&self.data_dir, &self.socket_path, false, None);
        self.child = child;
        self.principal_token = principal_token;
        self.wait_until_ready();
    }

    fn restart_at_memory_time(&mut self, memory_now_epoch: i64) {
        self.child.kill().expect("daemon should stop");
        self.child.wait().expect("daemon should be reaped");
        let (child, principal_token) = Self::spawn(
            &self.data_dir,
            &self.socket_path,
            true,
            Some(memory_now_epoch),
        );
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
    let project = create_project(temp.path(), "tyrion");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-source");
    let proposal_path = temp.path().join("source-proposal.json");
    write_proposal(
        &proposal_path,
        "project-tyrion",
        &project,
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
    assert_eq!(claim["token_accounting"], "utf8_byte_upper_bound");
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
    let project = create_project(temp.path(), "tyrion");
    let unrelated_project = create_project(temp.path(), "unrelated");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-loop");

    let source_path = temp.path().join("source-proposal.json");
    write_proposal_with_goal(
        &source_path,
        "project-tyrion",
        &project,
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
        &project,
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
        &unrelated_project,
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
    assert_eq!(budget["accounting"], "utf8_byte_upper_bound");
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
    assert_eq!(
        later_completed["results"][0]["profile_claim_outcomes"],
        json!([{
            "claim_id": claim_id,
            "claim_version": 1,
            "outcome": "accepted",
            "recorded_at": affected[0]["recorded_at"],
        }])
    );
    assert_eq!(
        later_completed["briefing"]["learning_receipts"],
        json!([{
            "kind": "profile_claim_applied",
            "claim_id": claim_id,
            "claim_version": 1,
            "attempt_id": later_completed["attempts"][0]["id"],
            "result_id": later_completed["results"][0]["id"],
            "outcome": "accepted",
        }])
    );

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
    let project = create_project(temp.path(), "tyrion");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-rework");

    let source_path = temp.path().join("rework-source.json");
    write_proposal(&source_path, "project-tyrion", &project, &[]);
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
    write_model_proposal(&later_path, "project-tyrion", &project);
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
        json!([
            {
                "kind": "profile_claim_applied_unsuccessfully",
                "claim_id": claim_id,
                "claim_version": 1,
                "attempt_id": completed["attempts"][0]["id"],
                "result_id": first_result,
                "outcome": "edited",
            },
            {
                "kind": "profile_claim_applied",
                "claim_id": claim_id,
                "claim_version": 1,
                "attempt_id": completed["attempts"][1]["id"],
                "result_id": second_result,
                "outcome": "accepted",
            }
        ])
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

#[test]
fn recovered_verifier_failure_accepts_the_same_influenced_result() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "tyrion");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-verifier-recovery");

    let source_path = temp.path().join("verifier-source.json");
    write_proposal(&source_path, "project-tyrion", &project, &[]);
    let source = create_proposal(
        &daemon,
        &attachment_token,
        &source_path,
        "create-verifier-source",
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
            "remember-verifier-preference",
        ],
    );
    let claim_id = remembered["claim"]["id"].as_str().unwrap().to_owned();

    let later_path = temp.path().join("verifier-later.json");
    write_model_proposal(&later_path, "project-tyrion", &project);
    let later = create_proposal(
        &daemon,
        &attachment_token,
        &later_path,
        "create-verifier-later",
    );
    let later_id = later["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        later_id,
        "accept-verifier-later",
    );
    daemon.restart_with_dispatch();
    let pending = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        later_id,
        "verification_pending",
        1,
    );
    let result_id = pending["results"][0]["id"].as_str().unwrap();
    let evidence_path = temp.path().join("verifier-evidence.json");
    write_model_evidence(&evidence_path, result_id, "failed", Some("verifier"));
    let failed = record_evidence(
        &daemon,
        &attachment_token,
        later_id,
        &evidence_path,
        "record-verifier-failure",
    );
    assert_eq!(
        failed["results"][0]["profile_claim_outcomes"][0]["outcome"],
        "rejected"
    );

    write_model_evidence(&evidence_path, result_id, "passed", None);
    record_evidence(
        &daemon,
        &attachment_token,
        later_id,
        &evidence_path,
        "record-verifier-recovery",
    );
    let completed = wait_for_status(&daemon, &attachment_token, later_id, "verified_complete");
    assert_eq!(
        completed["results"][0]["profile_claim_outcomes"][0]["outcome"],
        "accepted"
    );
    assert_eq!(
        completed["briefing"]["learning_receipts"],
        json!([{
            "kind": "profile_claim_applied",
            "claim_id": claim_id,
            "claim_version": 1,
            "attempt_id": completed["attempts"][0]["id"],
            "result_id": result_id,
            "outcome": "accepted",
        }])
    );

    let cancelled_path = temp.path().join("cancelled-later.json");
    write_model_proposal(&cancelled_path, "project-tyrion", &project);
    let cancellation = create_proposal(
        &daemon,
        &attachment_token,
        &cancelled_path,
        "create-cancelled-later",
    );
    let cancellation_id = cancellation["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        cancellation_id,
        "accept-cancelled-later",
    );
    let pending = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        cancellation_id,
        "verification_pending",
        1,
    );
    let cancelled_result_id = pending["results"][0]["id"].clone();
    let cancelled = run_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "commission",
            "cancel",
            cancellation_id,
            "--expected-revision",
            "1",
            "--idempotency-key",
            "cancel-influenced-result",
        ],
    );
    assert_eq!(
        cancelled["results"][0]["profile_claim_outcomes"],
        json!([{
            "claim_id": claim_id,
            "claim_version": 1,
            "outcome": "rejected",
            "recorded_at": cancelled["results"][0]["profile_claim_outcomes"][0]["recorded_at"],
        }])
    );
    assert_eq!(cancelled["results"][0]["id"], cancelled_result_id);
}

#[test]
fn inferred_project_preference_requires_independent_commission_support() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "tyrion");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "inferred-project-learning");

    let first_path = temp.path().join("inferred-first.json");
    write_proposal(&first_path, "project-tyrion", &project, &[]);
    let first = create_proposal(
        &daemon,
        &attachment_token,
        &first_path,
        "create-inferred-first",
    );
    let first_id = first["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        first_id,
        "accept-inferred-first",
    );

    let second_path = temp.path().join("inferred-second.json");
    write_proposal(&second_path, "project-tyrion", &project, &[]);
    let second = create_proposal(
        &daemon,
        &attachment_token,
        &second_path,
        "create-inferred-second",
    );
    let second_id = second["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        second_id,
        "accept-inferred-second",
    );

    let third_path = temp.path().join("inferred-third.json");
    write_proposal(&third_path, "project-tyrion", &project, &[]);
    let third = create_proposal(
        &daemon,
        &attachment_token,
        &third_path,
        "create-inferred-third",
    );
    let third_id = third["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        third_id,
        "accept-inferred-third",
    );

    daemon.restart_with_dispatch();
    wait_for_status(&daemon, &attachment_token, first_id, "verified_complete");
    wait_for_status(&daemon, &attachment_token, second_id, "verified_complete");
    wait_for_status(&daemon, &attachment_token, third_id, "verified_complete");

    let weak = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            first_id,
            "--statement",
            "Prefer focused integration tests for memory behavior.",
            "--outcome",
            "unedited-acceptance",
            "--idempotency-key",
            "observe-weak-preference",
        ],
    );
    assert_eq!(weak["claim"], Value::Null);
    assert_eq!(weak["observation"]["strength"], "weak");

    let first_strong = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            first_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--outcome",
            "principal-edit",
            "--idempotency-key",
            "observe-first-strong-preference",
        ],
    );
    let claim_id = first_strong["claim"]["id"].as_str().unwrap().to_owned();
    assert_eq!(first_strong["claim"]["strength"], "soft");
    assert_eq!(first_strong["claim"]["confidence"]["category"], "inferred");
    assert_eq!(first_strong["claim"]["lifecycle"]["state"], "candidate");
    assert_eq!(first_strong["support"]["independent_commissions"], 1);
    assert_eq!(first_strong["promoted"], false);

    let promoted = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            second_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--outcome",
            "explained-rejection",
            "--explanation",
            "The result tested private helpers instead of public behavior.",
            "--idempotency-key",
            "observe-second-strong-preference",
        ],
    );
    assert_eq!(promoted["claim"]["id"], claim_id);
    assert_eq!(promoted["claim"]["lifecycle"]["state"], "active");
    assert_eq!(promoted["support"]["independent_commissions"], 2);
    assert_eq!(promoted["support"]["includes_principal_signal"], true);
    assert_eq!(promoted["promoted"], true);

    daemon.restart();
    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    assert_eq!(inspected["claim"]["lifecycle"]["state"], "active");
    assert_eq!(inspected["observations"].as_array().unwrap().len(), 2);
    assert_eq!(inspected["lifecycle_history"].as_array().unwrap().len(), 2);

    let contradiction_candidate = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            first_id,
            "--statement",
            "Prefer contradiction-aware promotion.",
            "--outcome",
            "principal-edit",
            "--idempotency-key",
            "observe-contradiction-candidate",
        ],
    );
    let contradiction_claim_id = contradiction_candidate["claim"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let contradicted = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            second_id,
            "--statement",
            "Prefer contradiction-aware promotion.",
            "--outcome",
            "contradiction",
            "--explanation",
            "Current project evidence contradicts this preference.",
            "--idempotency-key",
            "observe-promotion-contradiction",
        ],
    );
    assert_eq!(contradicted["claim"]["lifecycle"]["state"], "contradicted");
    let blocked_promotion = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            third_id,
            "--statement",
            "Prefer contradiction-aware promotion.",
            "--outcome",
            "principal-edit",
            "--idempotency-key",
            "observe-after-promotion-contradiction",
        ],
    );
    assert_eq!(blocked_promotion["promoted"], false);
    assert_eq!(blocked_promotion["support"]["material_contradictions"], 1);
    assert_eq!(
        blocked_promotion["claim"]["lifecycle"]["state"],
        "contradicted"
    );
    let contradiction_history = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &contradiction_claim_id],
    );
    assert_eq!(
        contradiction_history["lifecycle_history"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn cross_project_preference_requires_explicit_principal_confirmation() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let first_project = create_project(temp.path(), "first-project");
    let second_project = create_project(temp.path(), "second-project");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "cross-project-learning");

    let mut commissions = Vec::new();
    for (position, (project_id, project)) in [
        ("project-first", &first_project),
        ("project-first", &first_project),
        ("project-second", &second_project),
        ("project-second", &second_project),
    ]
    .into_iter()
    .enumerate()
    {
        let proposal_path = temp.path().join(format!("cross-project-{position}.json"));
        write_proposal(&proposal_path, project_id, project, &[]);
        let created = create_proposal(
            &daemon,
            &attachment_token,
            &proposal_path,
            &format!("create-cross-project-{position}"),
        );
        let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
        accept_commission(
            &daemon,
            &attachment_token,
            &commission_id,
            &format!("accept-cross-project-{position}"),
        );
        commissions.push(commission_id);
    }
    daemon.restart_with_dispatch();
    for commission_id in &commissions {
        wait_for_status(
            &daemon,
            &attachment_token,
            commission_id,
            "verified_complete",
        );
    }

    let mut last_observation = Value::Null;
    for (position, commission_id) in commissions[..3].iter().enumerate() {
        last_observation = run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer behavior-first tests at public seams.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-cross-project-{position}"),
            ],
        );
    }

    let principal_candidate = &last_observation["principal_candidate"];
    let claim_id = principal_candidate["id"].as_str().unwrap().to_owned();
    assert_eq!(principal_candidate["scope"], json!({"kind": "principal"}));
    assert_eq!(principal_candidate["strength"], "soft");
    assert_eq!(principal_candidate["lifecycle"]["state"], "candidate");
    assert_eq!(
        last_observation["wider_scope"]["independent_commissions"],
        3
    );
    assert_eq!(last_observation["wider_scope"]["independent_projects"], 2);
    assert_eq!(
        last_observation["wider_scope"]["requires_confirmation"],
        true
    );

    let confirmed = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "confirm-preference",
            &commissions[2],
            &claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "confirm-cross-project-preference",
        ],
    );
    assert_eq!(confirmed["claim"]["lifecycle"]["state"], "active");
    assert_eq!(
        confirmed["learning_receipt"]["kind"],
        "profile_claim_confirmed"
    );

    daemon.restart();
    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    assert_eq!(inspected["claim"]["scope"], json!({"kind": "principal"}));
    assert_eq!(inspected["observations"].as_array().unwrap().len(), 3);
    assert_eq!(inspected["lifecycle_history"].as_array().unwrap().len(), 2);

    let contradicted_principal = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            &commissions[3],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--outcome",
            "contradiction",
            "--explanation",
            "Current project evidence materially contradicts the wider preference.",
            "--idempotency-key",
            "contradict-cross-project-preference",
        ],
    );
    assert_eq!(
        contradicted_principal["principal_candidate"]["lifecycle"]["state"],
        "contradicted"
    );
    let cannot_reconfirm = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "confirm-preference",
            &commissions[3],
            &claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "reject-contradicted-principal-preference",
        ],
    );
    assert!(!cannot_reconfirm.status.success());

    let mut forgotten_project_claim_id = String::new();
    let mut surviving_project_claim_id = String::new();
    let mut derived_candidate_id = String::new();
    for (position, commission_id) in commissions[..3].iter().enumerate() {
        let observed = run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer narrow derived-memory cleanup.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-derived-cleanup-{position}"),
            ],
        );
        if position == 0 {
            forgotten_project_claim_id = observed["claim"]["id"].as_str().unwrap().to_owned();
        }
        if position == 2 {
            surviving_project_claim_id = observed["claim"]["id"].as_str().unwrap().to_owned();
            derived_candidate_id = observed["principal_candidate"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
        }
    }
    let correction_preview = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            &commissions[0],
            &forgotten_project_claim_id,
            "--statement",
            "Prefer scoped derived-memory cleanup.",
            "--expected-version",
            "1",
            "--idempotency-key",
            "preview-derived-candidate-correction",
        ],
    );
    run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            &commissions[0],
            &forgotten_project_claim_id,
            "--statement",
            "Prefer scoped derived-memory cleanup.",
            "--expected-version",
            "1",
            "--confirmation-digest",
            correction_preview["confirmation_digest"].as_str().unwrap(),
            "--idempotency-key",
            "confirm-derived-candidate-correction",
        ],
    );
    let preview = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "forget-preference",
            &commissions[0],
            &forgotten_project_claim_id,
            "--expected-version",
            "2",
            "--idempotency-key",
            "preview-derived-cleanup",
        ],
    );
    assert_eq!(preview["cascade"]["claims"], 2);
    assert_eq!(preview["cascade"]["supporting_observations"], 3);
    assert_eq!(preview["cascade"]["dedicated_excerpts"], 2);
    assert_eq!(
        preview["remaining_related_claim_ids"],
        json!([surviving_project_claim_id.clone()])
    );
    run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "forget-preference",
            &commissions[0],
            &forgotten_project_claim_id,
            "--expected-version",
            "2",
            "--confirmation-digest",
            preview["confirmation_digest"].as_str().unwrap(),
            "--idempotency-key",
            "confirm-derived-cleanup",
        ],
    );
    for deleted_claim_id in [&forgotten_project_claim_id, &derived_candidate_id] {
        let missing = run_principal_cli_output(
            &daemon,
            None,
            &["principal", "inspect-claim", deleted_claim_id],
        );
        assert!(!missing.status.success());
    }
    let surviving = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &surviving_project_claim_id],
    );
    assert_eq!(surviving["observations"].as_array().unwrap().len(), 1);
}

#[test]
fn scope_strength_contradiction_and_decay_control_visible_memory() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "lifecycle-project");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "memory-lifecycle");

    let principal_path = temp.path().join("principal-memory.json");
    write_unscoped_proposal(&principal_path);
    let principal_source = create_proposal(
        &daemon,
        &attachment_token,
        &principal_path,
        "create-principal-memory",
    );
    let principal_source_id = principal_source["commission"]["id"].as_str().unwrap();
    run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            principal_source_id,
            "--statement",
            "Prefer concise commit messages.",
            "--idempotency-key",
            "remember-principal-preference",
        ],
    );

    let hard_path = temp.path().join("project-hard-memory.json");
    write_proposal(&hard_path, "project-lifecycle", &project, &[]);
    let hard_source = create_proposal(
        &daemon,
        &attachment_token,
        &hard_path,
        "create-project-hard-memory",
    );
    let hard_source_id = hard_source["commission"]["id"].as_str().unwrap();
    let hard = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            hard_source_id,
            "--statement",
            "Prefer public API tests.",
            "--idempotency-key",
            "remember-project-hard-preference",
        ],
    );
    let hard_claim_id = hard["claim"]["id"].as_str().unwrap().to_owned();

    let mut learning_commissions = Vec::new();
    for position in 0..3 {
        let path = temp
            .path()
            .join(format!("lifecycle-learning-{position}.json"));
        write_proposal(&path, "project-lifecycle", &project, &[]);
        let created = create_proposal(
            &daemon,
            &attachment_token,
            &path,
            &format!("create-lifecycle-learning-{position}"),
        );
        let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
        accept_commission(
            &daemon,
            &attachment_token,
            &commission_id,
            &format!("accept-lifecycle-learning-{position}"),
        );
        learning_commissions.push(commission_id);
    }
    daemon.restart_with_dispatch();
    for commission_id in &learning_commissions {
        wait_for_status(
            &daemon,
            &attachment_token,
            commission_id,
            "verified_complete",
        );
    }

    let mut promoted = Value::Null;
    for (position, commission_id) in learning_commissions[..2].iter().enumerate() {
        promoted = run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer behavior-first tests at public seams.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-lifecycle-preference-{position}"),
            ],
        );
    }
    let soft_claim_id = promoted["claim"]["id"].as_str().unwrap().to_owned();

    let later_path = temp.path().join("lifecycle-later.json");
    write_proposal(&later_path, "project-lifecycle", &project, &[]);
    let later = create_proposal(
        &daemon,
        &attachment_token,
        &later_path,
        "create-lifecycle-later",
    );
    let later_id = later["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        later_id,
        "accept-lifecycle-later",
    );
    daemon.restart_with_dispatch();
    let completed = wait_for_status(&daemon, &attachment_token, later_id, "verified_complete");
    let statements = completed["attempts"][0]["worker_context_packet"]["advisory"]
        ["profile_claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|claim| claim["statement"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        statements,
        [
            "Prefer public API tests.",
            "Prefer behavior-first tests at public seams.",
            "Prefer concise commit messages.",
        ]
    );

    let contradicted = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            &learning_commissions[2],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--outcome",
            "contradiction",
            "--explanation",
            "Current repository evidence requires a lower-level harness check.",
            "--idempotency-key",
            "contradict-lifecycle-preference",
        ],
    );
    assert_eq!(contradicted["claim"]["lifecycle"]["state"], "contradicted");
    let inspected = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &soft_claim_id],
    );
    assert_eq!(inspected["claim"]["lifecycle"]["state"], "contradicted");
    assert_eq!(inspected["lifecycle_history"].as_array().unwrap().len(), 3);
    let correction_preview = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            &learning_commissions[2],
            &soft_claim_id,
            "--statement",
            "Prefer repository-aligned tests at public seams.",
            "--expected-version",
            "1",
            "--idempotency-key",
            "preview-contradiction-correction",
        ],
    );
    let corrected = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            &learning_commissions[2],
            &soft_claim_id,
            "--statement",
            "Prefer repository-aligned tests at public seams.",
            "--expected-version",
            "1",
            "--confirmation-digest",
            correction_preview["confirmation_digest"].as_str().unwrap(),
            "--idempotency-key",
            "confirm-contradiction-correction",
        ],
    );
    assert_eq!(corrected["claim"]["version"], 2);
    assert_eq!(corrected["claim"]["lifecycle"]["state"], "active");
    let corrected = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &soft_claim_id],
    );
    assert_eq!(corrected["versions"][0]["disposition"], "superseded");
    assert_eq!(corrected["versions"][1]["disposition"], "current");
    assert_eq!(
        corrected["lifecycle_history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["reason"],
        "principal_correction"
    );
    let corrected_export = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "export-memory",
            "--project-id",
            "project-lifecycle",
        ],
    );
    let corrected_export = corrected_export["data"]["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["claim"]["id"] == soft_claim_id)
        .unwrap();
    assert_eq!(
        corrected_export["retention"]["last_nonweak_support_at"],
        corrected_export["retention"]["lifecycle_changed_at"]
    );

    for (position, commission_id) in learning_commissions[..2].iter().enumerate() {
        run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer deterministic fixtures.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-decaying-preference-{position}"),
            ],
        );
    }
    let decaying_profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-lifecycle",
        ],
    );
    let decaying_claim_id = decaying_profile["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|claim| claim["statement"] == "Prefer deterministic fixtures.")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    daemon.restart_at_memory_time(now + 181 * 24 * 60 * 60);
    let decayed = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &decaying_claim_id],
    );
    assert_eq!(decayed["claim"]["lifecycle"]["state"], "candidate");
    assert_eq!(
        decayed["lifecycle_history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["reason"],
        "soft_claim_decay"
    );
    let hard_after_decay = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &hard_claim_id],
    );
    assert_eq!(hard_after_decay["claim"]["lifecycle"]["state"], "active");
}

#[test]
fn suppression_forgetting_and_learning_boundaries_are_distinct_controls() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "control-project");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "memory-controls");

    let mut commissions = Vec::new();
    for position in 0..3 {
        let path = temp.path().join(format!("memory-control-{position}.json"));
        write_proposal(&path, "project-controls", &project, &[]);
        let created = create_proposal(
            &daemon,
            &attachment_token,
            &path,
            &format!("create-memory-control-{position}"),
        );
        let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
        accept_commission(
            &daemon,
            &attachment_token,
            &commission_id,
            &format!("accept-memory-control-{position}"),
        );
        commissions.push(commission_id);
    }
    daemon.restart_with_dispatch();
    for commission_id in &commissions {
        wait_for_status(
            &daemon,
            &attachment_token,
            commission_id,
            "verified_complete",
        );
    }

    let mut learned = Value::Null;
    for (position, commission_id) in commissions[..2].iter().enumerate() {
        learned = run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer behavior-first tests at public seams.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-control-preference-{position}"),
            ],
        );
    }
    let claim_id = learned["claim"]["id"].as_str().unwrap().to_owned();

    let boundary_before_forgetting = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "prevent-preference",
            &commissions[1],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "reject-boundary-before-forgetting",
        ],
    );
    assert!(!boundary_before_forgetting.status.success());
    assert!(String::from_utf8_lossy(&boundary_before_forgetting.stderr)
        .contains("forget matching Profile Claim"));

    let suppressed = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "suppress-preference",
            &commissions[1],
            &claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "suppress-control-preference",
        ],
    );
    assert_eq!(suppressed["claim"]["lifecycle"]["state"], "suppressed");
    assert_eq!(
        suppressed["learning_receipt"]["kind"],
        "profile_claim_suppressed"
    );
    let profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-controls",
        ],
    );
    assert_eq!(profile["claims"][0]["id"], claim_id);
    assert_eq!(profile["claims"][0]["lifecycle"]["state"], "suppressed");

    let preview = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "forget-preference",
            &commissions[1],
            &claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "preview-forget-control-preference",
        ],
    );
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["cascade"]["claim_versions"], 1);
    assert_eq!(preview["cascade"]["supporting_observations"], 2);
    assert_eq!(preview["cascade"]["dedicated_excerpts"], 2);
    assert_eq!(preview["cascade"]["indexes"], 1);
    assert_eq!(preview["cascade"]["caches"], 1);
    let confirmation_digest = preview["confirmation_digest"].as_str().unwrap();
    let forgotten = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "forget-preference",
            &commissions[1],
            &claim_id,
            "--expected-version",
            "1",
            "--confirmation-digest",
            confirmation_digest,
            "--idempotency-key",
            "confirm-forget-control-preference",
        ],
    );
    assert_eq!(forgotten["applied"], true);
    assert_eq!(forgotten["deletion_receipt"]["claim_id"], claim_id);
    assert!(forgotten["deletion_receipt"].get("statement").is_none());
    let missing =
        run_principal_cli_output(&daemon, None, &["principal", "inspect-claim", &claim_id]);
    assert!(!missing.status.success());

    let boundary = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "prevent-preference",
            &commissions[1],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "prevent-control-preference",
        ],
    );
    assert_eq!(
        boundary["boundary"]["scope"],
        json!({"kind": "project", "project_id": "project-controls"})
    );
    assert!(boundary["boundary"].get("statement").is_none());

    let duplicate_boundary = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "prevent-preference",
            &commissions[2],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "reject-duplicate-control-boundary",
        ],
    );
    assert!(!duplicate_boundary.status.success());
    assert!(String::from_utf8_lossy(&duplicate_boundary.stderr).contains("already exists"));

    let blocked_explicit_reactivation = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            &commissions[2],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "blocked-explicit-control-preference",
        ],
    );
    assert!(!blocked_explicit_reactivation.status.success());
    assert!(
        String::from_utf8_lossy(&blocked_explicit_reactivation.stderr)
            .contains("Learning Boundary")
    );

    let blocked = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            &commissions[2],
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--outcome",
            "principal-edit",
            "--idempotency-key",
            "blocked-control-preference",
        ],
    );
    assert_eq!(blocked["blocked_by_learning_boundary"], true);
    assert_eq!(blocked["claim"], Value::Null);
    assert_eq!(blocked["observation"], Value::Null);

    daemon.restart();
    let profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-controls",
        ],
    );
    assert_eq!(profile["claims"], json!([]));
    assert_eq!(profile["learning_boundaries"].as_array().unwrap().len(), 1);
    assert_eq!(profile["deletion_receipts"].as_array().unwrap().len(), 1);
    assert!(profile["deletion_receipts"][0].get("statement").is_none());
}

#[test]
fn profile_admission_evicts_soft_memory_without_truncating_hard_preferences() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "budget-project");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "memory-budgets");

    let mut commissions = Vec::new();
    for position in 0..2 {
        let path = temp
            .path()
            .join(format!("memory-budget-source-{position}.json"));
        write_proposal(&path, "project-budgets", &project, &[]);
        let created = create_proposal(
            &daemon,
            &attachment_token,
            &path,
            &format!("create-memory-budget-source-{position}"),
        );
        let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
        accept_commission(
            &daemon,
            &attachment_token,
            &commission_id,
            &format!("accept-memory-budget-source-{position}"),
        );
        commissions.push(commission_id);
    }
    daemon.restart_with_dispatch();
    for commission_id in &commissions {
        wait_for_status(
            &daemon,
            &attachment_token,
            commission_id,
            "verified_complete",
        );
    }

    let mut inferred = Value::Null;
    for (position, commission_id) in commissions.iter().enumerate() {
        inferred = run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer deterministic fixtures.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-memory-budget-{position}"),
            ],
        );
    }
    let soft_claim_id = inferred["claim"]["id"].as_str().unwrap().to_owned();
    assert_eq!(inferred["claim"]["lifecycle"]["state"], "active");

    for position in 0..50 {
        let statement = format!("Prefer {position:0<72}.");
        assert_eq!(statement.len(), 80);
        run_principal_cli(
            &daemon,
            Some(&attachment_token),
            &[
                "principal",
                "remember-preference",
                &commissions[0],
                "--statement",
                &statement,
                "--idempotency-key",
                &format!("remember-budget-hard-{position}"),
            ],
        );
    }

    let profile = run_principal_cli(
        &daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-budgets",
        ],
    );
    assert_eq!(profile["active_budget"]["claim_limit"], 80);
    assert_eq!(profile["active_budget"]["token_limit"], 4_000);
    assert_eq!(profile["active_budget"]["active_claims"], 50);
    assert_eq!(profile["active_budget"]["tokens_used"], 4_000);
    let evicted = profile["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|claim| claim["id"] == soft_claim_id)
        .unwrap();
    assert_eq!(evicted["lifecycle"]["state"], "candidate");
    let evicted = run_principal_cli(
        &daemon,
        None,
        &["principal", "inspect-claim", &soft_claim_id],
    );
    assert_eq!(
        evicted["lifecycle_history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["reason"],
        "active_profile_capacity"
    );

    let overflow_statement = format!("Prefer {:0<72}.", 50);
    let overflow = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            &commissions[0],
            "--statement",
            &overflow_statement,
            "--idempotency-key",
            "reject-hard-profile-overflow",
        ],
    );
    assert!(!overflow.status.success());
    assert!(String::from_utf8_lossy(&overflow.stderr).contains("hard Profile Claims"));

    let later_path = temp.path().join("memory-budget-later.json");
    write_proposal(&later_path, "project-budgets", &project, &[]);
    let later = create_proposal(
        &daemon,
        &attachment_token,
        &later_path,
        "create-memory-budget-later",
    );
    let later_id = later["commission"]["id"].as_str().unwrap();
    accept_commission(
        &daemon,
        &attachment_token,
        later_id,
        "accept-memory-budget-later",
    );
    daemon.restart_with_dispatch();
    let completed = wait_for_status(&daemon, &attachment_token, later_id, "verified_complete");
    let packet = &completed["attempts"][0]["worker_context_packet"]["advisory"];
    assert!(packet["budget"]["tokens_used"].as_u64().unwrap() <= 2_000);
    assert!(packet["profile_claims"]
        .as_array()
        .unwrap()
        .iter()
        .all(|claim| claim["statement"].as_str().unwrap().len() == 80));
}

#[test]
fn scoped_memory_export_import_is_portable_and_excludes_secrets() {
    let source_temp = TempDir::new().expect("temporary directory should be created");
    let source_data = source_temp.path().join("data");
    fs::create_dir(&source_data).unwrap();
    let source_project = create_project(source_temp.path(), "portable-source");
    let mut source_daemon = RunningDaemon::start(&source_data);
    let source_attachment = connect_full_entry(&source_daemon, "memory-export");
    let source_path = source_temp.path().join("portable-source.json");
    write_proposal(&source_path, "project-portable", &source_project, &[]);
    let source = create_proposal(
        &source_daemon,
        &source_attachment,
        &source_path,
        "create-portable-source",
    );
    let source_id = source["commission"]["id"].as_str().unwrap();
    let remembered = run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "remember-portable-preference",
        ],
    );
    let claim_id = remembered["claim"]["id"].as_str().unwrap().to_owned();
    let disposable = run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            "Prefer temporary export fixtures.",
            "--idempotency-key",
            "remember-disposable-export-preference",
        ],
    );
    let disposable_claim_id = disposable["claim"]["id"].as_str().unwrap();
    let forget_preview = run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "forget-preference",
            source_id,
            disposable_claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "preview-forget-disposable-export-preference",
        ],
    );
    run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "forget-preference",
            source_id,
            disposable_claim_id,
            "--expected-version",
            "1",
            "--confirmation-digest",
            forget_preview["confirmation_digest"].as_str().unwrap(),
            "--idempotency-key",
            "forget-disposable-export-preference",
        ],
    );
    run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "prevent-preference",
            source_id,
            "--statement",
            "Prefer temporary export fixtures.",
            "--idempotency-key",
            "prevent-portable-preference",
        ],
    );
    accept_commission(
        &source_daemon,
        &source_attachment,
        source_id,
        "accept-portable-source",
    );
    source_daemon.restart_with_dispatch();
    wait_for_status(
        &source_daemon,
        &source_attachment,
        source_id,
        "verified_complete",
    );

    let exported = run_principal_cli(
        &source_daemon,
        None,
        &[
            "principal",
            "export-memory",
            "--project-id",
            "project-portable",
        ],
    );
    assert_eq!(exported["format"], "tyrion.memory");
    assert_eq!(exported["version"], 1);
    assert_eq!(
        exported["scope"],
        json!({"kind": "project", "project_id": "project-portable"})
    );
    assert!(exported["checksum"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(exported["data"]["claims"][0]["claim"]["id"], claim_id);
    assert_eq!(
        exported["data"]["claims"][0]["versions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        exported["data"]["learning_boundaries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        exported["data"]["deletion_receipts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        exported["data"]["commission_records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let summary = exported["summary_markdown"].as_str().unwrap();
    assert!(summary.contains("# Tyrion Memory Export"));
    assert!(summary.contains(&claim_id));
    assert!(summary.contains(exported["checksum"].as_str().unwrap()));
    let serialized = serde_json::to_string(&exported).unwrap().to_lowercase();
    for prohibited in [
        "principal_control_token",
        "attachment_token",
        "session_token_hash",
        "credential_grants",
        "raw_secret",
    ] {
        assert!(!serialized.contains(prohibited));
    }

    let blocked_temp = TempDir::new().expect("temporary directory should be created");
    let blocked_data = blocked_temp.path().join("data");
    fs::create_dir(&blocked_data).unwrap();
    let blocked_project = create_project(blocked_temp.path(), "portable-blocked");
    let blocked_daemon = RunningDaemon::start(&blocked_data);
    let blocked_attachment = connect_full_entry(&blocked_daemon, "memory-import-boundary");
    let blocked_anchor_path = blocked_temp.path().join("portable-blocked-anchor.json");
    write_proposal(
        &blocked_anchor_path,
        "project-portable",
        &blocked_project,
        &[],
    );
    let blocked_anchor = create_proposal(
        &blocked_daemon,
        &blocked_attachment,
        &blocked_anchor_path,
        "create-portable-blocked-anchor",
    );
    let blocked_anchor_id = blocked_anchor["commission"]["id"].as_str().unwrap();
    run_principal_cli(
        &blocked_daemon,
        Some(&blocked_attachment),
        &[
            "principal",
            "prevent-preference",
            blocked_anchor_id,
            "--statement",
            "Prefer behavior-first tests at public seams.",
            "--idempotency-key",
            "prevent-portable-memory-import",
        ],
    );
    let blocked_export_path = blocked_temp.path().join("blocked-memory-export.json");
    fs::write(
        &blocked_export_path,
        serde_json::to_vec_pretty(&exported).unwrap(),
    )
    .unwrap();
    let blocked_import = run_principal_cli_output(
        &blocked_daemon,
        Some(&blocked_attachment),
        &[
            "principal",
            "import-memory",
            blocked_anchor_id,
            "--file",
            path_text(&blocked_export_path),
            "--idempotency-key",
            "reject-boundary-memory-import",
        ],
    );
    assert!(!blocked_import.status.success());
    assert!(String::from_utf8_lossy(&blocked_import.stderr).contains("Learning Boundary"));

    let destination_temp = TempDir::new().expect("temporary directory should be created");
    let destination_data = destination_temp.path().join("data");
    fs::create_dir(&destination_data).unwrap();
    let destination_project = create_project(destination_temp.path(), "portable-destination");
    let mut destination_daemon = RunningDaemon::start(&destination_data);
    let destination_attachment = connect_full_entry(&destination_daemon, "memory-import");
    let anchor_path = destination_temp.path().join("portable-anchor.json");
    write_proposal(&anchor_path, "project-portable", &destination_project, &[]);
    let anchor = create_proposal(
        &destination_daemon,
        &destination_attachment,
        &anchor_path,
        "create-portable-anchor",
    );
    let anchor_id = anchor["commission"]["id"].as_str().unwrap();
    for prohibited_field in [
        "secret",
        "api_key",
        "password",
        "access_token",
        "refresh_token",
        "authorization",
        "private_key",
    ] {
        let prohibited_export_path = destination_temp
            .path()
            .join(format!("prohibited-{prohibited_field}-memory-export.json"));
        let mut prohibited_export = exported.clone();
        prohibited_export["data"]["commission_records"][0][prohibited_field] =
            json!("must-not-import");
        fs::write(
            &prohibited_export_path,
            serde_json::to_vec_pretty(&prohibited_export).unwrap(),
        )
        .unwrap();
        let idempotency_key = format!("reject-{prohibited_field}-memory-import");
        let prohibited_import = run_principal_cli_output(
            &destination_daemon,
            Some(&destination_attachment),
            &[
                "principal",
                "import-memory",
                anchor_id,
                "--file",
                path_text(&prohibited_export_path),
                "--idempotency-key",
                &idempotency_key,
            ],
        );
        assert!(!prohibited_import.status.success());
        assert!(String::from_utf8_lossy(&prohibited_import.stderr).contains("prohibited secret"));
    }
    let malformed_export_path = destination_temp.path().join("malformed-memory-export.json");
    let mut malformed_export = exported.clone();
    malformed_export["data"]["claims"][0]["lifecycle_history"] = json!("missing");
    fs::write(
        &malformed_export_path,
        serde_json::to_vec_pretty(&malformed_export).unwrap(),
    )
    .unwrap();
    let malformed_import = run_principal_cli_output(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            anchor_id,
            "--file",
            path_text(&malformed_export_path),
            "--idempotency-key",
            "reject-malformed-memory-import",
        ],
    );
    assert!(!malformed_import.status.success());
    assert!(String::from_utf8_lossy(&malformed_import.stderr).contains("must be an array"));
    let export_path = destination_temp.path().join("memory-export.json");
    fs::write(&export_path, serde_json::to_vec_pretty(&exported).unwrap()).unwrap();
    let imported = run_principal_cli(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            anchor_id,
            "--file",
            path_text(&export_path),
            "--idempotency-key",
            "import-portable-memory",
        ],
    );
    assert_eq!(imported["checksum"], exported["checksum"]);
    assert_eq!(imported["imported"]["claims"], 1);
    assert_eq!(imported["imported"]["learning_boundaries"], 1);
    assert_eq!(imported["imported"]["deletion_receipts"], 1);
    assert_eq!(imported["imported"]["commission_records"], 1);

    destination_daemon.restart();
    let profile = run_principal_cli(
        &destination_daemon,
        None,
        &[
            "principal",
            "inspect-profile",
            "--project-id",
            "project-portable",
        ],
    );
    assert_eq!(profile["claims"].as_array().unwrap().len(), 1);
    assert_eq!(profile["claims"][0]["id"], claim_id);
    assert_eq!(
        profile["claims"][0]["statement"],
        "Prefer behavior-first tests at public seams."
    );
    assert_eq!(profile["learning_boundaries"].as_array().unwrap().len(), 1);
    assert_eq!(profile["deletion_receipts"].as_array().unwrap().len(), 1);

    accept_commission(
        &destination_daemon,
        &destination_attachment,
        anchor_id,
        "accept-portable-anchor",
    );
    destination_daemon.restart_with_dispatch();
    let completed = wait_for_status(
        &destination_daemon,
        &destination_attachment,
        anchor_id,
        "verified_complete",
    );
    assert_eq!(
        completed["attempts"][0]["worker_context_packet"]["advisory"]["profile_claims"][0]["id"],
        claim_id
    );
}

#[test]
fn principal_memory_round_trip_preserves_cross_project_provenance() {
    let source_temp = TempDir::new().expect("temporary directory should be created");
    let source_data = source_temp.path().join("data");
    fs::create_dir(&source_data).unwrap();
    let first_project = create_project(source_temp.path(), "portable-principal-first");
    let second_project = create_project(source_temp.path(), "portable-principal-second");
    let mut source_daemon = RunningDaemon::start(&source_data);
    let source_attachment = connect_full_entry(&source_daemon, "principal-memory-export");
    let mut commissions = Vec::new();
    for (position, (project_id, project)) in [
        ("project-principal-first", &first_project),
        ("project-principal-first", &first_project),
        ("project-principal-second", &second_project),
    ]
    .into_iter()
    .enumerate()
    {
        let path = source_temp
            .path()
            .join(format!("principal-portable-{position}.json"));
        write_proposal(&path, project_id, project, &[]);
        let created = create_proposal(
            &source_daemon,
            &source_attachment,
            &path,
            &format!("create-principal-portable-{position}"),
        );
        let commission_id = created["commission"]["id"].as_str().unwrap().to_owned();
        accept_commission(
            &source_daemon,
            &source_attachment,
            &commission_id,
            &format!("accept-principal-portable-{position}"),
        );
        commissions.push(commission_id);
    }
    source_daemon.restart_with_dispatch();
    for commission_id in &commissions {
        wait_for_status(
            &source_daemon,
            &source_attachment,
            commission_id,
            "verified_complete",
        );
    }
    let mut observed = Value::Null;
    for (position, commission_id) in commissions.iter().enumerate() {
        observed = run_principal_cli(
            &source_daemon,
            Some(&source_attachment),
            &[
                "principal",
                "observe-preference",
                commission_id,
                "--statement",
                "Prefer portable Principal memory.",
                "--outcome",
                "principal-edit",
                "--idempotency-key",
                &format!("observe-principal-portable-{position}"),
            ],
        );
    }
    let principal_claim_id = observed["principal_candidate"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    run_principal_cli(
        &source_daemon,
        Some(&source_attachment),
        &[
            "principal",
            "confirm-preference",
            &commissions[2],
            &principal_claim_id,
            "--expected-version",
            "1",
            "--idempotency-key",
            "confirm-principal-portable",
        ],
    );
    let applied_path = source_temp.path().join("principal-portable-applied.json");
    write_proposal(
        &applied_path,
        "project-principal-second",
        &second_project,
        &[],
    );
    let applied = create_proposal(
        &source_daemon,
        &source_attachment,
        &applied_path,
        "create-principal-portable-applied",
    );
    let applied_id = applied["commission"]["id"].as_str().unwrap();
    accept_commission(
        &source_daemon,
        &source_attachment,
        applied_id,
        "accept-principal-portable-applied",
    );
    source_daemon.restart_with_dispatch();
    wait_for_status(
        &source_daemon,
        &source_attachment,
        applied_id,
        "verified_complete",
    );
    let principal_export = run_principal_cli(&source_daemon, None, &["principal", "export-memory"]);
    assert_eq!(
        principal_export["data"]["commission_records"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        principal_export["data"]["claims"][0]["observations"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        principal_export["data"]["claims"][0]["affected_attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let project_export = run_principal_cli(
        &source_daemon,
        None,
        &[
            "principal",
            "export-memory",
            "--project-id",
            "project-principal-first",
        ],
    );
    let project_claim_id = project_export["data"]["claims"][0]["claim"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let destination_temp = TempDir::new().expect("temporary directory should be created");
    let destination_data = destination_temp.path().join("data");
    fs::create_dir(&destination_data).unwrap();
    let destination_daemon = RunningDaemon::start(&destination_data);
    let destination_attachment = connect_full_entry(&destination_daemon, "principal-memory-import");
    let principal_anchor_path = destination_temp.path().join("principal-import-anchor.json");
    write_unscoped_proposal(&principal_anchor_path);
    let principal_anchor = create_proposal(
        &destination_daemon,
        &destination_attachment,
        &principal_anchor_path,
        "create-principal-import-anchor",
    );
    let principal_anchor_id = principal_anchor["commission"]["id"].as_str().unwrap();
    let principal_export_path = destination_temp.path().join("principal-memory-export.json");
    fs::write(
        &principal_export_path,
        serde_json::to_vec_pretty(&principal_export).unwrap(),
    )
    .unwrap();
    for malformed_kind in ["claim-version", "outcome", "duplicate-attempt"] {
        let mut malformed_export = principal_export.clone();
        match malformed_kind {
            "claim-version" => {
                malformed_export["data"]["claims"][0]["affected_attempts"][0]["claim_version"] =
                    json!(99);
            }
            "outcome" => {
                malformed_export["data"]["claims"][0]["affected_attempts"][0]["outcome"] =
                    json!("invented");
            }
            "duplicate-attempt" => {
                let duplicate =
                    malformed_export["data"]["claims"][0]["affected_attempts"][0].clone();
                malformed_export["data"]["claims"][0]["affected_attempts"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }
            _ => unreachable!(),
        }
        rechecksum_memory_bundle(&mut malformed_export);
        let malformed_path = destination_temp
            .path()
            .join(format!("malformed-{malformed_kind}-principal-memory.json"));
        fs::write(
            &malformed_path,
            serde_json::to_vec_pretty(&malformed_export).unwrap(),
        )
        .unwrap();
        let malformed_import = run_principal_cli_output(
            &destination_daemon,
            Some(&destination_attachment),
            &[
                "principal",
                "import-memory",
                principal_anchor_id,
                "--file",
                path_text(&malformed_path),
                "--idempotency-key",
                &format!("reject-malformed-{malformed_kind}-attempt"),
            ],
        );
        assert!(!malformed_import.status.success());
        let stderr = String::from_utf8_lossy(&malformed_import.stderr);
        assert!(
            stderr.contains(match malformed_kind {
                "claim-version" => "references a missing claim version",
                "outcome" => "outcome is invalid",
                "duplicate-attempt" => "repeats affected Attempt",
                _ => unreachable!(),
            }),
            "unexpected import rejection: {stderr}"
        );
    }
    run_principal_cli(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            principal_anchor_id,
            "--file",
            path_text(&principal_export_path),
            "--idempotency-key",
            "import-principal-portable",
        ],
    );
    let imported_principal = run_principal_cli(
        &destination_daemon,
        None,
        &["principal", "inspect-claim", &principal_claim_id],
    );
    assert_eq!(
        imported_principal["observations"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        imported_principal["lifecycle_history"][0]["observation_id"],
        principal_export["data"]["claims"][0]["lifecycle_history"][0]["observation_id"]
    );
    assert_eq!(
        imported_principal["affected_attempts"],
        principal_export["data"]["claims"][0]["affected_attempts"]
    );

    let local_first_project = create_project(destination_temp.path(), "local-principal-first");
    let project_anchor_path = destination_temp.path().join("project-import-anchor.json");
    write_proposal(
        &project_anchor_path,
        "project-principal-first",
        &local_first_project,
        &[],
    );
    let project_anchor = create_proposal(
        &destination_daemon,
        &destination_attachment,
        &project_anchor_path,
        "create-project-import-anchor",
    );
    let project_anchor_id = project_anchor["commission"]["id"].as_str().unwrap();
    let project_export_path = destination_temp.path().join("project-memory-export.json");
    fs::write(
        &project_export_path,
        serde_json::to_vec_pretty(&project_export).unwrap(),
    )
    .unwrap();
    run_principal_cli(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            project_anchor_id,
            "--file",
            path_text(&project_export_path),
            "--idempotency-key",
            "import-project-after-principal",
        ],
    );
    let imported_project = run_principal_cli(
        &destination_daemon,
        None,
        &["principal", "inspect-claim", &project_claim_id],
    );
    assert_eq!(
        imported_project["observations"].as_array().unwrap().len(),
        2
    );

    let mut conflicting_record_export = project_export.clone();
    conflicting_record_export["data"]["commission_records"][0]["status"] = json!("cancelled");
    rechecksum_memory_bundle(&mut conflicting_record_export);
    let conflicting_record_path = destination_temp
        .path()
        .join("conflicting-commission-record-export.json");
    fs::write(
        &conflicting_record_path,
        serde_json::to_vec_pretty(&conflicting_record_export).unwrap(),
    )
    .unwrap();
    let conflicting_record_import = run_principal_cli_output(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            project_anchor_id,
            "--file",
            path_text(&conflicting_record_path),
            "--idempotency-key",
            "reject-conflicting-commission-record",
        ],
    );
    assert!(!conflicting_record_import.status.success());
    assert!(String::from_utf8_lossy(&conflicting_record_import.stderr)
        .contains("conflicts with existing immutable provenance"));

    let mut conflicting_export = project_export.clone();
    let conflicting_claim_id = "conflicting-imported-claim";
    let prior_claim_id = conflicting_export["data"]["claims"][0]["claim"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    conflicting_export["data"]["claims"][0]["claim"]["id"] = json!(conflicting_claim_id);
    let conflicting_statement = "Prefer conflicting imported provenance.";
    conflicting_export["data"]["claims"][0]["observations"][0]["statement"] =
        json!(conflicting_statement);
    conflicting_export["data"]["claims"][0]["observations"][0]["statement_fingerprint"] =
        json!(format!(
            "sha256:{:x}",
            Sha256::digest(conflicting_statement.to_lowercase().as_bytes())
        ));
    conflicting_export["summary_markdown"] = json!(conflicting_export["summary_markdown"]
        .as_str()
        .unwrap()
        .replace(&prior_claim_id, conflicting_claim_id));
    rechecksum_memory_bundle(&mut conflicting_export);
    let conflicting_export_path = destination_temp
        .path()
        .join("conflicting-memory-export.json");
    fs::write(
        &conflicting_export_path,
        serde_json::to_vec_pretty(&conflicting_export).unwrap(),
    )
    .unwrap();
    let conflicting_import = run_principal_cli_output(
        &destination_daemon,
        Some(&destination_attachment),
        &[
            "principal",
            "import-memory",
            project_anchor_id,
            "--file",
            path_text(&conflicting_export_path),
            "--idempotency-key",
            "reject-conflicting-observation-import",
        ],
    );
    assert!(!conflicting_import.status.success());
    assert!(String::from_utf8_lossy(&conflicting_import.stderr)
        .contains("conflicts with existing immutable provenance"));
}

#[test]
fn terminal_temporary_material_expires_without_erasing_durable_records() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "retention-project");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "memory-retention");

    let expiring_path = temp.path().join("retention-expiring.json");
    write_proposal(&expiring_path, "project-retention", &project, &[]);
    let expiring = create_proposal(
        &daemon,
        &attachment_token,
        &expiring_path,
        "create-retention-expiring",
    );
    let expiring_id = expiring["commission"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        &expiring_id,
        "accept-retention-expiring",
    );

    let pinned_path = temp.path().join("retention-pinned.json");
    write_proposal(&pinned_path, "project-retention", &project, &[]);
    let pinned = create_proposal(
        &daemon,
        &attachment_token,
        &pinned_path,
        "create-retention-pinned",
    );
    let pinned_id = pinned["commission"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        &pinned_id,
        "accept-retention-pinned",
    );

    let learned_path = temp.path().join("retention-learned.json");
    write_proposal(&learned_path, "project-retention", &project, &[]);
    let learned = create_proposal(
        &daemon,
        &attachment_token,
        &learned_path,
        "create-retention-learned",
    );
    let learned_id = learned["commission"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        &learned_id,
        "accept-retention-learned",
    );

    let cancelled_path = temp.path().join("retention-cancelled.json");
    write_model_proposal(&cancelled_path, "project-retention", &project);
    let cancelled = create_proposal(
        &daemon,
        &attachment_token,
        &cancelled_path,
        "create-retention-cancelled",
    );
    let cancelled_id = cancelled["commission"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        &cancelled_id,
        "accept-retention-cancelled",
    );

    let active_path = temp.path().join("retention-active.json");
    write_model_proposal(&active_path, "project-retention", &project);
    let active = create_proposal(
        &daemon,
        &attachment_token,
        &active_path,
        "create-retention-active",
    );
    let active_id = active["commission"]["id"].as_str().unwrap().to_owned();
    accept_commission(
        &daemon,
        &attachment_token,
        &active_id,
        "accept-retention-active",
    );

    daemon.restart_with_dispatch();
    let expiring = wait_for_status(
        &daemon,
        &attachment_token,
        &expiring_id,
        "verified_complete",
    );
    let durable_evidence_count = expiring["evidence"].as_array().unwrap().len();
    let pinned = wait_for_status(&daemon, &attachment_token, &pinned_id, "verified_complete");
    wait_for_status(&daemon, &attachment_token, &learned_id, "verified_complete");
    run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "observe-preference",
            &learned_id,
            "--statement",
            "Prefer retention tied to learned provenance.",
            "--outcome",
            "principal-edit",
            "--idempotency-key",
            "observe-retained-learning-source",
        ],
    );
    wait_for_assignment_status(
        &daemon,
        &attachment_token,
        &cancelled_id,
        "verification_pending",
        1,
    );
    let active = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        &active_id,
        "verification_pending",
        1,
    );
    assert_eq!(expiring["retention_materials"].as_array().unwrap().len(), 1);
    let expiring_transcript = &expiring["retention_materials"][0];
    assert_eq!(expiring_transcript["captured_worker_output"], true);
    assert!(expiring_transcript["content_bytes"].as_u64().unwrap() > 0);
    let pinned_transcript_id = pinned["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|material| material["kind"] == "raw_worker_transcript")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "pin-memory-material",
            &pinned_id,
            pinned_transcript_id,
            "--idempotency-key",
            "pin-retention-transcript",
        ],
    );
    assert!(active["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .all(|material| material["expires_at"].is_null()));

    run_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "commission",
            "cancel",
            &cancelled_id,
            "--expected-revision",
            "1",
            "--idempotency-key",
            "cancel-retention-commission",
        ],
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    daemon.restart_at_memory_time(now + 31 * 24 * 60 * 60);

    let expiring = run_cli(
        &daemon,
        Some(&attachment_token),
        &["commission", "inspect", &expiring_id],
    );
    assert!(expiring["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .all(|material| material["content_available"] == false
            && material["content_bytes"] == 0
            && material["captured_worker_output"] == false));
    assert!(expiring["briefing"].is_object());
    assert_eq!(expiring["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        expiring["evidence"].as_array().unwrap().len(),
        durable_evidence_count
    );

    let pinned = run_cli(
        &daemon,
        Some(&attachment_token),
        &["commission", "inspect", &pinned_id],
    );
    let pinned_transcript = pinned["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|material| material["kind"] == "raw_worker_transcript")
        .unwrap();
    assert_eq!(pinned_transcript["pinned"], true);
    assert_eq!(pinned_transcript["content_available"], true);

    let learned = run_cli(
        &daemon,
        Some(&attachment_token),
        &["commission", "inspect", &learned_id],
    );
    let learned_transcript = learned["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|material| material["kind"] == "raw_worker_transcript")
        .unwrap();
    assert_eq!(learned_transcript["retained_by_claim"], true);
    assert_eq!(learned_transcript["content_available"], true);

    let cancelled = run_cli(
        &daemon,
        Some(&attachment_token),
        &["commission", "inspect", &cancelled_id],
    );
    let unaccepted = cancelled["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|material| material["kind"] == "unaccepted_artifact")
        .unwrap();
    assert_eq!(unaccepted["content_available"], false);
    assert_eq!(cancelled["commission"]["status"], "cancelled");
    assert_eq!(cancelled["results"].as_array().unwrap().len(), 1);

    let active = run_cli(
        &daemon,
        Some(&attachment_token),
        &["commission", "inspect", &active_id],
    );
    assert_eq!(active["commission"]["status"], "active");
    assert!(active["retention_materials"]
        .as_array()
        .unwrap()
        .iter()
        .all(|material| material["content_available"] == true && material["expires_at"].is_null()));
}

#[test]
fn project_identity_atomicity_and_claim_changes_are_enforced() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let project = create_project(temp.path(), "tyrion");
    let impostor = create_project(temp.path(), "impostor");
    let mut daemon = RunningDaemon::start(&data_dir);
    let attachment_token = connect_full_entry(&daemon, "learning-controls");

    let source_path = temp.path().join("controls-source.json");
    write_proposal(&source_path, "project-tyrion", &project, &[]);
    let source = create_proposal(
        &daemon,
        &attachment_token,
        &source_path,
        "create-controls-source",
    );
    let source_id = source["commission"]["id"].as_str().unwrap();

    let compound = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            "Prefer public tests while minimizing private helpers.",
            "--idempotency-key",
            "reject-compound-preference",
        ],
    );
    assert!(!compound.status.success());
    assert!(String::from_utf8_lossy(&compound.stderr).contains("one atomic sentence"));

    let oversized_statement = "x".repeat(81);
    let oversized = run_principal_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "remember-preference",
            source_id,
            "--statement",
            &oversized_statement,
            "--idempotency-key",
            "reject-oversized-preference",
        ],
    );
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("conservative UTF-8 accounting"));

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
            "remember-controls-preference",
        ],
    );
    let claim_id = remembered["claim"]["id"].as_str().unwrap();
    let preview = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            source_id,
            claim_id,
            "--statement",
            "Prefer end-to-end tests at public seams.",
            "--expected-version",
            "1",
            "--idempotency-key",
            "preview-controls-preference",
        ],
    );
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["expected_version"], 1);
    assert_eq!(
        preview["diff"]["statement"],
        json!({
            "before": "Prefer behavior-first tests at public seams.",
            "after": "Prefer end-to-end tests at public seams.",
        })
    );
    let confirmation_digest = preview["confirmation_digest"].as_str().unwrap();
    let revised = run_principal_cli(
        &daemon,
        Some(&attachment_token),
        &[
            "principal",
            "revise-preference",
            source_id,
            claim_id,
            "--statement",
            "Prefer end-to-end tests at public seams.",
            "--expected-version",
            "1",
            "--confirmation-digest",
            confirmation_digest,
            "--idempotency-key",
            "revise-controls-preference",
        ],
    );
    assert_eq!(revised["applied"], true);
    assert_eq!(revised["claim"]["version"], 2);
    assert_eq!(revised["learning_receipt"]["kind"], "profile_claim_changed");
    assert_eq!(revised["learning_receipt"]["previous_version"], 1);
    assert_eq!(revised["learning_receipt"]["claim_version"], 2);

    let inspected = run_principal_cli(&daemon, None, &["principal", "inspect-claim", claim_id]);
    assert_eq!(inspected["claim"]["version"], 2);
    assert_eq!(inspected["versions"].as_array().unwrap().len(), 2);
    assert_eq!(inspected["versions"][0]["disposition"], "superseded");
    assert_eq!(inspected["versions"][1]["disposition"], "current");

    let impostor_path = temp.path().join("impostor-proposal.json");
    write_proposal(&impostor_path, "project-tyrion", &impostor, &[]);
    let output = run_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "proposal",
            "create",
            "--file",
            path_text(&impostor_path),
            "--idempotency-key",
            "reject-project-impostor",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different repository identity"));

    accept_commission(
        &daemon,
        &attachment_token,
        source_id,
        "accept-controls-source",
    );
    daemon.restart_with_dispatch();
    let completed = wait_for_status(&daemon, &attachment_token, source_id, "verified_complete");
    assert_eq!(
        completed["briefing"]["learning_receipts"],
        json!([
            {
                "kind": "profile_claim_created",
                "claim_id": claim_id,
                "claim_version": 1,
                "scope": {"kind": "project", "project_id": "project-tyrion"},
            },
            {
                "kind": "profile_claim_changed",
                "claim_id": claim_id,
                "previous_version": 1,
                "claim_version": 2,
                "scope": {"kind": "project", "project_id": "project-tyrion"},
            }
        ])
    );
}

fn write_proposal(path: &Path, project_id: &str, project: &Path, constraints: &[&str]) {
    write_proposal_with_goal(
        path,
        project_id,
        project,
        "return a deterministic greeting",
        constraints,
    );
}

fn write_unscoped_proposal(path: &Path) {
    let proposal = json!({
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
            "max_storage_bytes": 4096,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_proposal_with_goal(
    path: &Path,
    project_id: &str,
    project: &Path,
    goal: &str,
    constraints: &[&str],
) {
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
            "repositories": [path_text(project)],
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

fn write_model_proposal(path: &Path, project_id: &str, project: &Path) {
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
            "repositories": [path_text(project)],
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
    successful_json(run_cli_output(daemon, attachment_token, arguments))
}

fn run_cli_output(
    daemon: &RunningDaemon,
    attachment_token: Option<&str>,
    arguments: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tyrion"));
    command.args(["--socket", path_text(&daemon.socket_path)]);
    if let Some(attachment_token) = attachment_token {
        command.args(["--attachment-token", attachment_token]);
    }
    command.args(arguments);
    command.output().expect("CLI should run")
}

fn run_principal_cli(
    daemon: &RunningDaemon,
    attachment_token: Option<&str>,
    arguments: &[&str],
) -> Value {
    successful_json(run_principal_cli_output(
        daemon,
        attachment_token,
        arguments,
    ))
}

fn run_principal_cli_output(
    daemon: &RunningDaemon,
    attachment_token: Option<&str>,
    arguments: &[&str],
) -> Output {
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
    child.wait_with_output().unwrap()
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

fn rechecksum_memory_bundle(bundle: &mut Value) {
    let prior_checksum = bundle["checksum"].as_str().unwrap().to_owned();
    let checksum = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&bundle["data"]).unwrap())
    );
    bundle["checksum"] = json!(checksum);
    let summary = bundle["summary_markdown"]
        .as_str()
        .unwrap()
        .replace(&prior_checksum, bundle["checksum"].as_str().unwrap());
    bundle["summary_markdown"] = json!(summary);
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

fn create_project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    fs::create_dir(&project).unwrap();
    project
}

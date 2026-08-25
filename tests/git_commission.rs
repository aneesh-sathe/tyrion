#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct RunningDaemon {
    child: Child,
    socket_path: PathBuf,
}

fn skill_version(name: &str) -> Value {
    let marker = if name == "backend" { "2" } else { "3" };
    json!({
        "name": name,
        "content_digest": format!("sha256:{}", marker.repeat(64)),
    })
}

impl RunningDaemon {
    fn start(data_dir: &Path, worker_config: &Path, _fake_state: &Path) -> Self {
        Self::start_with_arguments(data_dir, worker_config, &[])
    }

    fn start_with_arguments(
        data_dir: &Path,
        worker_config: &Path,
        extra_arguments: &[&str],
    ) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(&socket_path),
                "--codex-worker-config",
                path_text(worker_config),
            ])
            .args(extra_arguments);
        let child = command.spawn().expect("daemon should start");
        let mut daemon = Self { child, socket_path };
        daemon.wait_until_ready();
        daemon
    }

    fn start_with_catalog(data_dir: &Path, worker_config: &Path, catalog: &Path) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_tyriond"))
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(&socket_path),
                "--codex-worker-config",
                path_text(worker_config),
                "--worker-catalog",
                path_text(catalog),
            ])
            .spawn()
            .expect("daemon should start");
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
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires the pinned repaired OpenShell gateway and a brokered Codex provider"]
fn real_openshell_microvm_completes_the_contained_git_assignment() {
    let worker_config = std::env::var_os("TYRION_REAL_CODEX_WORKER_CONFIG")
        .map(PathBuf::from)
        .expect("set TYRION_REAL_CODEX_WORKER_CONFIG to the pinned runtime JSON");
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let sibling_checkout = temp.path().join("sibling-checkout");
    fs::create_dir(&sibling_checkout).unwrap();
    fs::write(sibling_checkout.join("principal-only.txt"), "unavailable\n").unwrap();
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &worker_config, temp.path());
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    set_proposal_ceiling(&proposal_path, "max_elapsed_seconds", 900);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let completed = wait_for_completion_with_timeout(
        &daemon,
        &attachment_token,
        &commission_id,
        Duration::from_secs(900),
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["results"][0]["status"], "accepted");
    assert_eq!(completed["attempts"][0]["lease"]["status"], "released");
    assert!(!principal_checkout.join("issue-4.txt").exists());
    assert_eq!(
        fs::read_to_string(sibling_checkout.join("principal-only.txt")).unwrap(),
        "unavailable\n"
    );
}

#[test]
fn contained_codex_result_is_verified_integrated_and_verified_again() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-openshell");
    fs::create_dir(&fake_state).unwrap();
    let fake_openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_openshell.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);

    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);

    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-git-commission",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-git-commission",
        ],
    );

    let completed = wait_for_completion(&daemon, &attachment_token, commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["assignments"][0]["status"], "accepted");
    assert_eq!(completed["attempts"][0]["status"], "succeeded");
    assert_eq!(completed["attempts"][0]["lease"]["status"], "released");
    assert!(completed["attempts"][0]["worker_configuration"]
        .as_str()
        .is_some_and(|configuration| configuration.starts_with("contained-codex-")));
    assert_eq!(completed["workers"][0]["handle"], "Arya");
    assert_eq!(
        completed["workers"][0]["configuration"]["adapter"]["kind"],
        "contained_codex"
    );
    assert_eq!(
        completed["workers"][0]["configuration"]["model"],
        "fixture-model"
    );
    assert_eq!(
        completed["workers"][0]["configuration"]["settings"]["vcpus"],
        2
    );
    assert_eq!(
        completed["workers"][0]["configuration"]["settings"]["runtime_configuration_sha256"],
        sha256_file(&runtime)
    );
    assert!(!completed["workers"][0]["configuration"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == "semantic_interrupt"));
    assert_eq!(
        completed["workers"][0]["elapsed_time_ms"],
        completed["attempts"][0]["execution_completed_at_ms"]
            .as_i64()
            .unwrap()
            - completed["attempts"][0]["started_at_ms"].as_i64().unwrap()
    );

    let result = &completed["results"][0];
    assert_eq!(result["status"], "accepted");
    assert_eq!(result["mandate_revision"], 1);
    assert_eq!(result["base_revision"], base_revision);
    assert_eq!(result["changed_paths"], json!(["issue-4.txt"]));
    assert_eq!(result["known_effects"], json!([]));
    assert_eq!(result["candidate_commits"].as_array().unwrap().len(), 1);
    assert_eq!(result["artifacts"].as_array().unwrap().len(), 3);
    assert_eq!(
        result["integrated_artifact_revision"],
        completed["commission"]["artifact_revision"]
    );
    let verification = result["verification_outcomes"].as_array().unwrap();
    assert_eq!(verification.len(), 2);
    assert!(verification
        .iter()
        .all(|outcome| outcome["outcome"] == "passed"));
    assert_eq!(verification[0]["scope"], "candidate");
    assert_eq!(verification[1]["scope"], "integrated");
    assert_eq!(completed["evidence"].as_array().unwrap().len(), 2);
    assert!(completed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|evidence| evidence["outcome"] == "passed"
            && evidence["artifact_revision"] == completed["commission"]["artifact_revision"]));
    assert!(!principal_checkout.join("issue-4.txt").exists());

    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap();
    assert_eq!(log.matches("sandbox create").count(), 3);
    assert_eq!(log.matches("sandbox delete").count(), 3);
    assert_eq!(log.matches("descendant-terminated").count(), 4);
    assert!(log.contains("--no-auto-providers"));
    assert!(log.contains("--cpu 2 --memory 2Gi"));
    assert!(log.contains("ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:"));
    assert!(log.contains("tyrion-containment-probe"));
    assert!(log.contains("descendant-terminated"));
    assert!(log.contains("/sandbox/codex --version"));
    assert!(!log.lines().any(|line| {
        line.contains("sandbox upload") && line.contains(path_text(&principal_checkout))
    }));
}

#[test]
fn restart_restores_unacknowledged_integration_before_retry() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-openshell");
    fs::create_dir(&fake_state).unwrap();
    let fake_openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_openshell.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let first = RunningDaemon::start_with_arguments(
        &data_dir,
        &runtime,
        &["--fault-hold-worker-after-external-integration"],
    );
    let attachment_token = connect_full_entry(&first);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    set_proposal_ceiling(&proposal_path, "max_attempts", 2);
    let commission_id = create_and_accept(&first, &attachment_token, &proposal_path);
    let integration_repository = data_dir
        .join("integrations")
        .join(&commission_id)
        .join("repository");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if integration_repository.exists() {
            let integration_revision = Command::new("git")
                .arg("-C")
                .arg(&integration_repository)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            if integration_revision.status.success()
                && String::from_utf8_lossy(&integration_revision.stdout).trim() != base_revision
            {
                let inspected = run_cli(
                    &first.socket_path,
                    &[
                        "--attachment-token",
                        &attachment_token,
                        "commission",
                        "inspect",
                        &commission_id,
                    ],
                );
                assert!(inspected["results"][0]["integrated_artifact_revision"].is_null());
                break;
            }
        }
        assert!(Instant::now() < deadline, "Integration was not mutated");
        thread::sleep(Duration::from_millis(20));
    }
    drop(first);

    let second =
        RunningDaemon::start_with_arguments(&data_dir, &runtime, &["--fault-defer-ready-dispatch"]);
    assert_eq!(
        git_output(&integration_repository, &["rev-parse", "HEAD"]).trim(),
        base_revision
    );
    let recovered = run_cli(
        &second.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            &commission_id,
        ],
    );
    assert_eq!(recovered["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(recovered["attempts"][0]["status"], "failed");
    assert_eq!(
        recovered["restart_recoveries"][0]["cleanup_confirmed"],
        true
    );
    assert_eq!(
        recovered["restart_recoveries"][0]["proofs"]["acknowledged_state"],
        false
    );
    drop(second);

    let third = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let deadline = Instant::now() + Duration::from_secs(10);
    let completed = loop {
        let inspected = run_cli(
            &third.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "commission",
                "inspect",
                &commission_id,
            ],
        );
        if inspected["commission"]["status"] == "verified_complete" {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "recovered Commission did not complete: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(completed["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(completed["commission"]["status"], "verified_complete");
}

#[test]
fn codex_and_claude_structured_adapters_complete_one_git_commission() {
    let fixture = ParallelFixture::new();
    add_claude_runtime_fixture(fixture.temp.path(), &fixture.runtime);
    let catalog = fixture.temp.path().join("structured-worker-catalog.json");
    let adapter_script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_structured_adapter.sh"
    );
    let configuration = |id: &str, harness: &str, kind: &str, skill: &str, score: u16| {
        json!({
            "id": id,
            "harness": harness,
            "adapter": {
                "kind": kind,
                "version": "contract-fixture-v1",
                "sha256": sha256_file(Path::new(adapter_script)),
                "command": [adapter_script, harness]
            },
            "model": format!("{harness}-fixture-model"),
            "settings": {"mode": "structured_git"},
            "tools": ["git"],
            "skills": [skill_version(skill)],
            "context": {"strategy": "fresh", "capacity_tokens": 100000},
            "resource_limits": {
                "max_concurrency_slots": 1,
                "max_storage_bytes": 5242880,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            },
            "capabilities": [
                "structured_lifecycle", "semantic_interrupt", "terminal_state", "usage",
                "skills", "result_submission", "contained"
            ],
            "authority_actions": ["codex.git_change"],
            "authority_scope_types": ["repository", "path", "action"],
            "assignment_constraints": ["coding"],
            "containment_profile": "openshell-repaired-v0.0.104",
            "replacement_class": "structured-git",
            "available": true,
            "metrics": {
                "expected_verified_correctness": score,
                "preference_adherence": 9000,
                "first_pass_acceptance": 9000,
                "commission_elapsed_time_contribution_ms": 1000,
                "cost_cents": 0,
                "continuity": 0
            }
        })
    };
    fs::write(
        &catalog,
        serde_json::to_vec_pretty(&json!({
            "configurations": [
                configuration("codex-structured-git", "codex", "codex_app_server", "backend", 9500),
                configuration("claude-structured-git", "claude", "claude_agent_sdk", "frontend", 9600)
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let proposal = fixture.temp.path().join("structured-git-proposal.json");
    write_parallel_git_proposal(
        &proposal,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut value: Value = serde_json::from_slice(&fs::read(&proposal).unwrap()).unwrap();
    value["plan"]["assignments"][0]["worker_requirements"] = json!({
        "capabilities": ["structured_lifecycle", "semantic_interrupt"],
        "tools": ["git"],
        "skills": [skill_version("backend")],
        "min_context_tokens": 100000,
        "assignment_constraints": ["coding"]
    });
    value["plan"]["assignments"][1]["worker_requirements"] = json!({
        "capabilities": ["structured_lifecycle", "semantic_interrupt"],
        "tools": ["git"],
        "skills": [skill_version("frontend")],
        "min_context_tokens": 100000,
        "assignment_constraints": ["coding"]
    });
    fs::write(&proposal, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let daemon = RunningDaemon::start_with_catalog(&fixture.data_dir, &fixture.runtime, &catalog);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);

    assert_eq!(completed["commission"]["status"], "verified_complete");
    let harnesses = completed["workers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|worker| worker["configuration"]["harness"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        harnesses,
        std::collections::HashSet::from(["codex", "claude"])
    );
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["status"] == "accepted"));
}

#[test]
fn disjoint_useful_assignments_run_concurrently_and_complete_the_assembled_artifact() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-openshell");
    fs::create_dir(&fake_state).unwrap();
    let fake_openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_openshell.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);

    let started = Instant::now();
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);

    let elapsed_millis = started.elapsed().as_millis() as u64;
    let serial_attempt_millis = completed["activity_journal"]["useful_concurrency"]
        ["serial_attempt_millis"]
        .as_u64()
        .unwrap();
    assert!(
        elapsed_millis < serial_attempt_millis,
        "parallel end-to-end time {elapsed_millis}ms did not beat the {serial_attempt_millis}ms serial Attempt time"
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["assignments"].as_array().unwrap().len(), 2);
    assert!(completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .all(|assignment| assignment["status"] == "accepted"));
    assert_eq!(completed["results"].as_array().unwrap().len(), 2);
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["status"] == "accepted"));
    assert!(completed["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .all(|criterion| criterion["status"] == "passed"));
    assert!(completed["plans"].as_array().unwrap().len() >= 2);
    assert!(
        completed["activity_journal"]["useful_concurrency"]["overlap_millis"]
            .as_u64()
            .is_some_and(|overlap| overlap > 0)
    );
    assert!(completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["type"] == "useful_concurrency_observed"));
    let concurrency = &completed["activity_journal"]["useful_concurrency"];
    assert!(concurrency["serial_execution_millis"].as_u64().unwrap() > 0);
    assert!(
        concurrency["parallel_execution_window_millis"]
            .as_u64()
            .unwrap()
            < concurrency["serial_execution_millis"].as_u64().unwrap()
    );
    assert!(concurrency["end_to_end_elapsed_millis"].as_u64().unwrap() > 0);
    let reservations = completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "resources_reserved")
        .collect::<Vec<_>>();
    assert_eq!(reservations.len(), 2);
    assert!(reservations
        .iter()
        .all(|event| event["payload"]["reserved_atomically"] == true));
}

#[test]
fn concurrent_read_only_assignments_verify_without_mutating_the_artifact() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("read-only-proposal.json");
    write_parallel_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    for (position, assignment) in proposal["plan"]["assignments"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        assignment["goal"] = json!(format!(
            "TYRION_FIXTURE_READ_ONLY=1 TYRION_FIXTURE_DELAY=1 inspect README pass {}",
            position + 1
        ));
        assignment["purpose"] = json!("independent_verification");
        assignment["read_scopes"] = json!(["README.md"]);
        assignment["write_scopes"] = json!([]);
    }
    for criterion in proposal["criteria"].as_array_mut().unwrap() {
        criterion["verifier"]["argv"] = json!(["sh", "-c", "test -f README.md"]);
    }
    proposal["authority"]["paths"] = json!(["README.md"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);

    assert_eq!(
        completed["commission"]["artifact_revision"],
        fixture.base_revision
    );
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["changed_paths"] == json!([])));
    assert_eq!(
        completed["activity_journal"]["useful_concurrency"]["occurred"],
        true
    );
}

#[test]
fn planned_spend_reservations_cannot_exceed_the_commission_ceiling() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("oversubscribed-spend.json");
    write_parallel_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(10);
    proposal["plan"]["assignments"][0]["resources"]["max_model_spend_cents"] = json!(6);
    proposal["plan"]["assignments"][1]["resources"]["max_model_spend_cents"] = json!(6);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "reject-oversubscribed-spend",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cumulative model spend"));
}

#[test]
fn competition_members_must_share_one_dependency_frontier() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture
        .temp
        .path()
        .join("invalid-competition-frontier.json");
    write_competing_conflict_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["plan"]["assignments"][1]["dependencies"] = json!(["backend"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "reject-invalid-competition-frontier",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("share one dependency frontier"));
}

#[test]
fn comparison_working_set_must_fit_the_commission_storage_ceiling() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("under-resourced-comparison.json");
    write_competing_conflict_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["resource_ceilings"]["max_storage_bytes"] = json!(10_485_760);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "reject-under-resourced-comparison",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("comparison working set"));
}

#[test]
fn declared_overlapping_writes_serialize_against_the_latest_integrated_revision() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("serialized-proposal.json");
    write_overlapping_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
        false,
    );
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let created = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-serialized-commission",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    let accepted = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-serialized-commission",
        ],
    );
    assert_eq!(accepted["execution_frontier"].as_array().unwrap().len(), 1);
    assert_eq!(accepted["frontier_holds"].as_array().unwrap().len(), 1);
    assert_eq!(
        accepted["frontier_holds"][0]["reason"],
        "declared_write_overlap"
    );

    let completed = wait_for_completion(&daemon, &attachment_token, commission_id);
    let attempts = completed["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(
        attempts[0]["completed_at_ms"].as_i64().unwrap()
            <= attempts[1]["started_at_ms"].as_i64().unwrap(),
        "declared overlapping writes ran concurrently: {attempts:?}"
    );
    assert_eq!(
        completed["activity_journal"]["useful_concurrency"]["occurred"],
        false
    );
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["status"] == "accepted"));
}

#[test]
fn unexpected_scope_overlap_creates_an_explicit_reconciliation_assignment() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("unexpected-overlap.json");
    write_parallel_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["plan"]["assignments"][0]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=frontend.txt TYRION_FIXTURE_CONTENT=unexpected TYRION_FIXTURE_DELAY=1"
    );
    proposal["criteria"][0]["verifier"]["argv"] = json!(["sh", "-c", "test -f frontend.txt"]);
    proposal["criteria"][1]["verifier"]["argv"] = json!(["sh", "-c", "test -f frontend.txt"]);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(10);
    proposal["resource_ceilings"]["max_paid_service_spend_cents"] = json!(10);
    for assignment in proposal["plan"]["assignments"].as_array_mut().unwrap() {
        assignment["resources"]["max_model_spend_cents"] = json!(3);
        assignment["resources"]["max_paid_service_spend_cents"] = json!(2);
    }
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let reconciled = wait_for_reconciliation(&daemon, &attachment_token, &commission_id);
    assert_eq!(reconciled["commission"]["status"], "active");
    let reconciliation = reconciled["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["purpose"] == "reconciliation")
        .expect("an explicit reconciliation Assignment should exist");
    assert_ne!(reconciliation["status"], "resource_blocked");
    assert_eq!(
        reconciliation["resources"],
        json!({
            "concurrency_slots": 1,
            "max_storage_bytes": 10_485_760,
            "max_model_spend_cents": 3,
            "max_paid_service_spend_cents": 2,
        })
    );
    let event = reconciled["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "reconciliation_required")
        .unwrap();
    assert_eq!(event["payload"]["kind"], "unexpected_overlap");
    assert_eq!(event["payload"]["silent_winner_selected"], false);
    assert!(reconciled["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["status"] == "candidate"
            && result["integrated_artifact_revision"].is_null()));
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(
        completed["activity_journal"]["useful_concurrency"]["occurred"],
        false
    );
}

#[test]
fn competing_writes_record_their_question_and_reconcile_an_integration_conflict() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("competing-conflict.json");
    write_competing_conflict_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let reconciled = wait_for_reconciliation(&daemon, &attachment_token, &commission_id);
    let competing = reconciled["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|assignment| {
            assignment["competition"].is_object() && assignment["purpose"] != "reconciliation"
        })
        .collect::<Vec<_>>();
    assert_eq!(competing.len(), 2);
    assert!(competing.iter().all(|assignment| {
        assignment["competition"]["uncertainty"] == "which implementation should own shared.txt"
            && assignment["competition"]["comparison_rule"]
                == "prefer the candidate that preserves assembled verification"
    }));
    let event = reconciled["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "reconciliation_required")
        .unwrap();
    assert_eq!(event["payload"]["kind"], "competition_comparison");
    assert_eq!(event["payload"]["silent_winner_selected"], false);
    assert_eq!(
        reconciled["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|result| result["status"] == "accepted")
            .count(),
        0
    );
    let reconciliation = reconciled["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["purpose"] == "reconciliation")
        .unwrap();
    assert_ne!(reconciliation["status"], "resource_blocked");
    assert_eq!(reconciliation["resources"]["max_storage_bytes"], 15_728_640);

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(
        completed["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|result| result["status"] == "accepted")
            .count(),
        1
    );
    assert_eq!(
        completed["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|result| result["status"] == "superseded")
            .count(),
        2
    );
    let log = fs::read_to_string(fixture.fake_state.join("commands.log")).unwrap();
    assert!(log.matches("/sandbox/contenders/").count() >= 2);
    assert_eq!(
        completed["activity_journal"]["useful_concurrency"]["occurred"],
        true
    );
}

#[test]
fn comparison_plan_snapshot_respects_active_resource_reservations() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("held-comparison.json");
    write_competing_conflict_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["criteria"].as_array_mut().unwrap().push(json!({
        "id": "other-file",
        "description": "The assembled repository contains unrelated work",
        "required_evidence": "command_output",
        "verifier_type": "deterministic",
        "verification_depth": "standard",
        "verifier": {
            "kind": "command",
            "argv": ["sh", "-c", "test -f other.txt"]
        }
    }));
    proposal["plan"]["assignments"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "other",
            "goal": "TYRION_FIXTURE_WRITE=other.txt TYRION_FIXTURE_CONTENT=other TYRION_FIXTURE_DELAY=5",
            "dependencies": [],
            "criterion_ids": ["other-file"],
            "purpose": "critical_path",
            "read_scopes": [],
            "write_scopes": ["other.txt"],
            "resources": {
                "concurrency_slots": 1,
                "max_storage_bytes": 5_242_880,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            }
        }));
    proposal["authority"]["paths"] = json!(["shared.txt", "other.txt"]);
    proposal["resource_ceilings"]["max_attempts"] = json!(4);
    proposal["resource_ceilings"]["max_worker_concurrency"] = json!(3);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let reconciled = wait_for_reconciliation(&daemon, &attachment_token, &commission_id);
    let comparison = reconciled["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["purpose"] == "reconciliation")
        .unwrap();
    let latest_plan = reconciled["plans"].as_array().unwrap().last().unwrap();
    assert!(latest_plan["snapshot"]["execution_frontier"]
        .as_array()
        .is_some_and(|frontier| !frontier.iter().any(|id| id == &comparison["logical_id"])));
    assert!(reconciled["frontier_holds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hold| hold["logical_id"] == comparison["logical_id"]
            && hold["reason"] == "complete_resource_budget_unavailable"));
}

#[test]
fn assembled_state_regression_rolls_back_before_reconciliation() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("integrated-regression.json");
    write_overlapping_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
        false,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["plan"]["assignments"][1]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=shared/frontend.txt TYRION_FIXTURE_CONTENT=frontend TYRION_FIXTURE_DELETE=shared/backend.txt TYRION_FIXTURE_DELAY=1"
    );
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let reconciled = wait_for_reconciliation(&daemon, &attachment_token, &commission_id);
    let event = reconciled["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "reconciliation_required")
        .unwrap();
    assert_eq!(event["payload"]["kind"], "integrated_regression");
    let accepted_revision = reconciled["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["status"] == "accepted")
        .unwrap()["integrated_artifact_revision"]
        .clone();
    assert_eq!(
        reconciled["commission"]["artifact_revision"],
        accepted_revision
    );
    assert!(reconciled["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["status"] == "candidate"
            && result["integrated_artifact_revision"].is_null()));
    let regressed_result_id = event["payload"]["source_result_id"].as_str().unwrap();
    let regressed_result = reconciled["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["id"] == regressed_result_id)
        .unwrap();
    assert!(regressed_result["verification_outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|outcome| outcome["scope"] == "integrated" && outcome["outcome"] == "failed"));
    assert!(reconciled["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|evidence| evidence["result_id"] == regressed_result_id
            && evidence["scope"] == "integrated"
            && evidence["outcome"] == "failed"));
}

#[test]
fn evidence_revision_exposes_a_new_dependency_satisfied_frontier() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("incremental-plan.json");
    write_incremental_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["plans"][0]["source"], "entry_model");
    assert_eq!(
        completed["plans"][0]["snapshot"]["assignments"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let assembly = completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["logical_id"] == "assembly")
        .unwrap();
    assert!(assembly["plan_revision"].as_i64().unwrap() > 1);
    assert!(completed["plans"].as_array().unwrap().iter().any(|plan| {
        plan["snapshot"]["execution_frontier"]
            .as_array()
            .is_some_and(|frontier| frontier.iter().any(|id| id == "assembly"))
    }));
    let assembly_attempt = completed["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attempt| attempt["assignment_id"] == assembly["id"])
        .unwrap();
    let final_concurrency_event = completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "useful_concurrency_observed")
        .next_back()
        .unwrap();
    assert_eq!(
        final_concurrency_event["payload"]["trigger_attempt_id"],
        assembly_attempt["id"]
    );
}

#[test]
fn evolving_plan_snapshot_excludes_work_held_by_a_running_overlap() {
    let fixture = ParallelFixture::new();
    let proposal_path = fixture.temp.path().join("held-incremental-plan.json");
    write_incremental_git_proposal(
        &proposal_path,
        &fixture.principal_checkout,
        &fixture.base_revision,
    );
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["plan"]["assignments"][1]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=frontend.txt TYRION_FIXTURE_CONTENT=frontend TYRION_FIXTURE_DELAY=5"
    );
    proposal["plan"]["assignments"][2]["dependencies"] = json!(["backend"]);
    proposal["plan"]["assignments"][2]["write_scopes"] = json!(["frontend.txt"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let inspected = wait_for_frontier_hold(&daemon, &attachment_token, &commission_id, "assembly");
    let latest_plan = inspected["plans"].as_array().unwrap().last().unwrap();
    assert!(
        latest_plan["snapshot"]["execution_frontier"]
            .as_array()
            .is_some_and(|frontier| !frontier.iter().any(|id| id == "assembly")),
        "held Assignment leaked into plan frontier: {latest_plan}"
    );
}

#[test]
fn failed_containment_preflight_revokes_the_lease_without_launching_codex() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-openshell");
    fs::create_dir(&fake_state).unwrap();
    fs::write(fake_state.join("fail-preflight"), b"").unwrap();
    let fake_openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_openshell.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["assignments"][0]["status"], "verification_failed");
    assert_eq!(failed["attempts"][0]["status"], "failed");
    assert_eq!(failed["attempts"][0]["lease"]["status"], "revoked");
    assert_eq!(failed["results"], json!([]));
    assert_eq!(failed["blockers"][0]["code"], "worker_execution_failed");
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("simulated containment failure"));
    assert!(!data_dir.join("integrations").exists());
    assert!(!principal_checkout.join("issue-4.txt").exists());

    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap();
    assert_eq!(log.matches("sandbox create").count(), 1);
    assert_eq!(log.matches("sandbox delete").count(), 1);
    assert!(!log.contains("run-attempt.sh"));
}

#[test]
fn guest_codex_version_mismatch_is_rejected_before_execution() {
    let fixture = FailedFixture::new("wrong-codex-version");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["assignments"][0]["status"], "verification_failed");
    assert_eq!(failed["attempts"][0]["status"], "failed");
    assert_eq!(failed["attempts"][0]["lease"]["status"], "revoked");
    assert_eq!(failed["results"], json!([]));
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("Codex binary version does not match its pin"));

    let log = fs::read_to_string(fixture.fake_state.join("commands.log")).unwrap();
    assert!(log.contains("/sandbox/codex --version"));
    assert!(!log.contains("run-attempt.sh"));
}

#[test]
fn malformed_returned_bundle_never_reaches_verification_or_integration() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-openshell");
    fs::create_dir(&fake_state).unwrap();
    fs::write(fake_state.join("corrupt-candidate"), b"").unwrap();
    let fake_openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_openshell.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["commission"]["status"], "active");
    assert_eq!(failed["attempts"][0]["lease"]["status"], "revoked");
    assert_eq!(failed["results"], json!([]));
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("Git failed"));
    assert!(!data_dir.join("integrations").exists());
    assert!(!principal_checkout.join("issue-4.txt").exists());

    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap();
    assert_eq!(log.matches("sandbox create").count(), 1);
    assert_eq!(log.matches("sandbox delete").count(), 1);
}

#[test]
fn unauthorized_changed_path_is_rejected_before_verification() {
    let fixture = FailedFixture::new("unauthorized-change");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["results"], json!([]));
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("unauthorized path outside.txt"));
    assert!(!fixture.data_dir.join("integrations").exists());
    assert!(!fixture.principal_checkout.join("outside.txt").exists());
}

#[test]
fn reverted_unauthorized_path_is_rejected_before_verification() {
    let fixture = FailedFixture::new("reverted-unauthorized-change");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["results"], json!([]));
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("unauthorized path outside.txt"));
    assert!(!fixture.data_dir.join("integrations").exists());
    assert!(!fixture.principal_checkout.join("outside.txt").exists());
}

#[test]
fn expired_worker_lease_deletes_the_sandbox_and_terminates_descendants() {
    let fixture = FailedFixture::new("slow-codex");
    set_lease_ttl(&fixture.runtime, 2);
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["attempts"][0]["lease"]["status"], "expired");
    assert_eq!(failed["results"], json!([]));
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("Worker Lease expired"));
    let log = fs::read_to_string(fixture.fake_state.join("commands.log")).unwrap();
    assert!(log.contains("sandbox delete"));
    assert!(log.contains("descendant-terminated"));
}

#[test]
fn slow_worker_does_not_block_the_control_plane_listener() {
    let fixture = FailedFixture::new("slow-codex");
    set_lease_ttl(&fixture.runtime, 2);
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let started = Instant::now();
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
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "listener was blocked for {:?}",
        started.elapsed()
    );
    assert_eq!(inspected["commission"]["status"], "active");
    wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
}

#[test]
fn watchdog_deletes_a_stalled_candidate_verification_sandbox() {
    let fixture = FailedFixture::new("hold-candidate-verification");
    set_proposal_ceiling(&fixture.proposal_path, "max_attempts", 2);
    let daemon = RunningDaemon::start_with_arguments(
        &fixture.data_dir,
        &fixture.runtime,
        &["--watchdog-stall-milliseconds", "1500"],
    );
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);
    let deadline = Instant::now() + Duration::from_secs(10);
    let contained = loop {
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
        if inspected["commission"]["status"] == "verified_complete" {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Watchdog did not contain candidate verification: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(contained["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(contained["attempts"][0]["status"], "timed_out");
    assert_eq!(contained["attempts"][0]["cleanup_pending"], false);
    assert_eq!(contained["attempts"][1]["status"], "succeeded");
    assert_eq!(contained["results"][0]["status"], "superseded");
    assert!(contained["results"][0]["integrated_artifact_revision"].is_null());
    assert!(contained["watchdog"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["signal"] == "stall"));
    let remaining_sandboxes = fs::read_dir(fixture.fake_state.join("sandboxes"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(remaining_sandboxes.is_empty());
    let log = fs::read_to_string(fixture.fake_state.join("commands.log")).unwrap();
    assert!(log.contains("descendant-terminated"));
}

#[test]
fn worker_storage_breach_is_a_resource_block_with_an_exact_requirement() {
    let fixture = FailedFixture::new("storage-ceiling");
    set_proposal_ceiling(&fixture.proposal_path, "max_storage_bytes", 1);
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let blocked = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(blocked["commission"]["status"], "active");
    assert_eq!(blocked["assignments"][0]["status"], "resource_blocked");
    assert_eq!(blocked["attempts"][0]["status"], "failed");
    assert_eq!(blocked["attempts"][0]["lease"]["status"], "revoked");
    assert_eq!(blocked["blockers"][0]["code"], "max_storage_bytes");
    assert!(blocked["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("require at least"));
}

#[test]
fn failed_fresh_integrated_verification_prevents_completion() {
    let fixture = FailedFixture::new("fail-integrated-verification");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_verification_failure(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["commission"]["status"], "active");
    assert!(failed["commission"]["artifact_revision"].is_string());
    assert_eq!(failed["assignments"][0]["status"], "verification_failed");
    assert_eq!(failed["attempts"][0]["status"], "succeeded");
    assert_eq!(failed["attempts"][0]["lease"]["status"], "released");
    assert_eq!(failed["results"][0]["status"], "candidate");
    assert!(failed["results"][0]["integrated_artifact_revision"].is_string());
    let outcomes = failed["results"][0]["verification_outcomes"]
        .as_array()
        .unwrap();
    assert_eq!(outcomes[0]["scope"], "candidate");
    assert_eq!(outcomes[0]["outcome"], "passed");
    assert_eq!(outcomes[1]["scope"], "integrated");
    assert_eq!(outcomes[1]["outcome"], "failed");
    assert_eq!(failed["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(failed["evidence"][0]["outcome"], "passed");
    assert_eq!(failed["evidence"][1]["outcome"], "failed");
    assert_eq!(failed["briefing"], Value::Null);
}

#[test]
fn unavailable_verifier_remains_uncertain_and_recommends_retry() {
    let fixture = FailedFixture::new("unused-marker");
    set_proposal_verifier(
        &fixture.proposal_path,
        &["/definitely-unavailable-verifier"],
    );
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let uncertain = wait_for_verification_failure(&daemon, &attachment_token, &commission_id);
    assert_eq!(uncertain["commission"]["status"], "active");
    assert_eq!(uncertain["assignments"][0]["status"], "verification_failed");
    assert_eq!(uncertain["results"][0]["status"], "candidate");
    assert_eq!(
        uncertain["evidence"][0]["outcome"], "uncertain",
        "{}",
        uncertain["evidence"][0]
    );
    assert_eq!(uncertain["evidence"][0]["defect"], "environment");
    assert_eq!(uncertain["verification"]["verdict"], "uncertain");
    assert_eq!(uncertain["verification"]["next_action"], "retry");
    assert_eq!(uncertain["briefing"], Value::Null);
}

struct FailedFixture {
    _temp: TempDir,
    principal_checkout: PathBuf,
    fake_state: PathBuf,
    runtime: PathBuf,
    data_dir: PathBuf,
    proposal_path: PathBuf,
}

struct ParallelFixture {
    temp: TempDir,
    principal_checkout: PathBuf,
    base_revision: String,
    fake_state: PathBuf,
    runtime: PathBuf,
    data_dir: PathBuf,
}

impl ParallelFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temporary directory should be created");
        let principal_checkout = temp.path().join("principal-checkout");
        let base_revision = create_principal_repository(&principal_checkout);
        let fake_state = temp.path().join("fake-openshell");
        fs::create_dir(&fake_state).unwrap();
        let fake_openshell = write_executable(
            &temp.path().join("openshell"),
            include_str!("fixtures/fake_openshell.sh"),
        );
        let fake_codex = write_executable(
            &temp.path().join("codex"),
            include_str!("fixtures/fake_codex.sh"),
        );
        let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
        let data_dir = temp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        Self {
            temp,
            principal_checkout,
            base_revision,
            fake_state,
            runtime,
            data_dir,
        }
    }
}

impl FailedFixture {
    fn new(marker: &str) -> Self {
        let temp = TempDir::new().expect("temporary directory should be created");
        let principal_checkout = temp.path().join("principal-checkout");
        let base_revision = create_principal_repository(&principal_checkout);
        let fake_state = temp.path().join("fake-openshell");
        fs::create_dir(&fake_state).unwrap();
        fs::write(fake_state.join(marker), b"").unwrap();
        let fake_openshell = write_executable(
            &temp.path().join("openshell"),
            include_str!("fixtures/fake_openshell.sh"),
        );
        let fake_codex = write_executable(
            &temp.path().join("codex"),
            include_str!("fixtures/fake_codex.sh"),
        );
        let runtime = write_runtime_fixture(temp.path(), &fake_openshell, &fake_codex);
        let data_dir = temp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let proposal_path = temp.path().join("proposal.json");
        write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
        Self {
            _temp: temp,
            principal_checkout,
            fake_state,
            runtime,
            data_dir,
            proposal_path,
        }
    }
}

fn set_lease_ttl(runtime: &Path, seconds: u64) {
    let mut config: Value = serde_json::from_slice(&fs::read(runtime).unwrap()).unwrap();
    config["lease_ttl_seconds"] = json!(seconds);
    fs::write(runtime, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

fn set_proposal_ceiling(path: &Path, key: &str, value: u64) {
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal["resource_ceilings"][key] = json!(value);
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn set_proposal_verifier(path: &Path, argv: &[&str]) {
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal["criteria"][0]["verifier"]["argv"] = json!(argv);
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_git_proposal(path: &Path, principal_checkout: &Path, base_revision: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "goal": "Add issue-4.txt containing contained codex result.",
            "execution": {
                "kind": "codex_git",
                "repository": principal_checkout,
                "base_revision": base_revision,
            },
            "criteria": [{
                "id": "issue-file",
                "description": "The integrated repository contains the requested file",
                "required_evidence": "command_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier": {
                    "kind": "command",
                    "argv": ["sh", "-c", "test \"$(cat issue-4.txt)\" = 'contained codex result'"]
                }
            }],
            "authority": {
                "repositories": [principal_checkout],
                "paths": ["issue-4.txt"],
                "actions": ["codex.git_change"],
                "destinations": [],
                "effects": []
            },
            "resource_ceilings": {
                "max_attempts": 1,
                "max_elapsed_seconds": 30,
                "max_worker_concurrency": 1,
                "max_storage_bytes": 10485760,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_parallel_git_proposal(path: &Path, principal_checkout: &Path, base_revision: &str) {
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "goal": "Add independently implemented backend and frontend artifacts.",
            "execution": {
                "kind": "codex_git",
                "repository": principal_checkout,
                "base_revision": base_revision,
            },
            "criteria": [
                {
                    "id": "backend-file",
                    "description": "The assembled repository contains the backend artifact",
                    "required_evidence": "command_output",
                    "verifier_type": "deterministic",
                    "verification_depth": "standard",
                    "verifier": {
                        "kind": "command",
                        "argv": ["sh", "-c", "test \"$(cat backend.txt)\" = backend"]
                    }
                },
                {
                    "id": "frontend-file",
                    "description": "The assembled repository contains the frontend artifact",
                    "required_evidence": "command_output",
                    "verifier_type": "deterministic",
                    "verification_depth": "standard",
                    "verifier": {
                        "kind": "command",
                        "argv": ["sh", "-c", "test \"$(cat frontend.txt)\" = frontend"]
                    }
                }
            ],
            "plan": {
                "assignments": [
                    {
                        "id": "backend",
                        "goal": "TYRION_FIXTURE_WRITE=backend.txt TYRION_FIXTURE_CONTENT=backend TYRION_FIXTURE_DELAY=1",
                        "dependencies": [],
                        "criterion_ids": ["backend-file"],
                        "purpose": "critical_path",
                        "read_scopes": [],
                        "write_scopes": ["backend.txt"],
                        "resources": {
                            "concurrency_slots": 1,
                            "max_storage_bytes": 5242880,
                            "max_model_spend_cents": 0,
                            "max_paid_service_spend_cents": 0
                        }
                    },
                    {
                        "id": "frontend",
                        "goal": "TYRION_FIXTURE_WRITE=frontend.txt TYRION_FIXTURE_CONTENT=frontend TYRION_FIXTURE_DELAY=1",
                        "dependencies": [],
                        "criterion_ids": ["frontend-file"],
                        "purpose": "critical_path",
                        "read_scopes": [],
                        "write_scopes": ["frontend.txt"],
                        "resources": {
                            "concurrency_slots": 1,
                            "max_storage_bytes": 5242880,
                            "max_model_spend_cents": 0,
                            "max_paid_service_spend_cents": 0
                        }
                    }
                ]
            },
            "authority": {
                "repositories": [principal_checkout],
                "paths": ["backend.txt", "frontend.txt"],
                "actions": ["codex.git_change"],
                "destinations": [],
                "effects": []
            },
            "resource_ceilings": {
                "max_attempts": 3,
                "max_elapsed_seconds": 30,
                "max_worker_concurrency": 2,
                "max_storage_bytes": 10485760,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_overlapping_git_proposal(
    path: &Path,
    principal_checkout: &Path,
    base_revision: &str,
    competing: bool,
) {
    write_parallel_git_proposal(path, principal_checkout, base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal["plan"]["assignments"][0]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=shared/backend.txt TYRION_FIXTURE_CONTENT=backend TYRION_FIXTURE_DELAY=1"
    );
    proposal["plan"]["assignments"][1]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=shared/frontend.txt TYRION_FIXTURE_CONTENT=frontend TYRION_FIXTURE_DELAY=1"
    );
    proposal["plan"]["assignments"][0]["write_scopes"] = json!(["shared"]);
    proposal["plan"]["assignments"][1]["write_scopes"] = json!(["shared"]);
    proposal["criteria"][0]["verifier"]["argv"] =
        json!(["sh", "-c", "test \"$(cat shared/backend.txt)\" = backend"]);
    proposal["criteria"][1]["verifier"]["argv"] =
        json!(["sh", "-c", "test \"$(cat shared/frontend.txt)\" = frontend"]);
    proposal["authority"]["paths"] = json!(["shared"]);
    if competing {
        let comparison = json!({
            "group": "shared-implementation",
            "uncertainty": "which isolated implementation best preserves the shared contract",
            "comparison_rule": "prefer the candidate whose assembled verification passes all contract checks"
        });
        proposal["plan"]["assignments"][0]["competition"] = comparison.clone();
        proposal["plan"]["assignments"][1]["competition"] = comparison;
    }
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_competing_conflict_proposal(path: &Path, principal_checkout: &Path, base_revision: &str) {
    write_parallel_git_proposal(path, principal_checkout, base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal["plan"]["assignments"][0]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=shared.txt TYRION_FIXTURE_CONTENT=first TYRION_FIXTURE_DELAY=1"
    );
    proposal["plan"]["assignments"][1]["goal"] = json!(
        "TYRION_FIXTURE_WRITE=shared.txt TYRION_FIXTURE_CONTENT=second TYRION_FIXTURE_DELAY=1"
    );
    proposal["plan"]["assignments"][0]["write_scopes"] = json!(["shared.txt"]);
    proposal["plan"]["assignments"][1]["write_scopes"] = json!(["shared.txt"]);
    let competition = json!({
        "group": "shared-owner",
        "uncertainty": "which implementation should own shared.txt",
        "comparison_rule": "prefer the candidate that preserves assembled verification"
    });
    proposal["plan"]["assignments"][0]["competition"] = competition.clone();
    proposal["plan"]["assignments"][1]["competition"] = competition;
    proposal["criteria"][0]["verifier"]["argv"] = json!(["sh", "-c", "test -f shared.txt"]);
    proposal["criteria"][1]["verifier"]["argv"] = json!(["sh", "-c", "test -f shared.txt"]);
    proposal["authority"]["paths"] = json!(["shared.txt"]);
    proposal["resource_ceilings"]["max_attempts"] = json!(3);
    proposal["resource_ceilings"]["max_storage_bytes"] = json!(15_728_640);
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn write_incremental_git_proposal(path: &Path, principal_checkout: &Path, base_revision: &str) {
    write_parallel_git_proposal(path, principal_checkout, base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal["criteria"].as_array_mut().unwrap().push(json!({
        "id": "assembly-file",
        "description": "The assembled repository records final assembly",
        "required_evidence": "command_output",
        "verifier_type": "deterministic",
        "verification_depth": "standard",
        "verifier": {
            "kind": "command",
            "argv": ["sh", "-c", "test \"$(cat assembly.txt)\" = assembled"]
        }
    }));
    proposal["plan"]["assignments"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "assembly",
            "goal": "TYRION_FIXTURE_WRITE=assembly.txt TYRION_FIXTURE_CONTENT=assembled",
            "dependencies": ["backend", "frontend"],
            "criterion_ids": ["assembly-file"],
            "purpose": "critical_path",
            "read_scopes": ["backend.txt", "frontend.txt"],
            "write_scopes": ["assembly.txt"],
            "resources": {
                "concurrency_slots": 1,
                "max_storage_bytes": 5242880,
                "max_model_spend_cents": 0,
                "max_paid_service_spend_cents": 0
            }
        }));
    proposal["authority"]["paths"] = json!(["backend.txt", "frontend.txt", "assembly.txt"]);
    proposal["resource_ceilings"]["max_attempts"] = json!(3);
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn create_principal_repository(path: &Path) -> String {
    fs::create_dir(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.name", "Tyrion Fixture"]);
    git(path, &["config", "user.email", "fixture@tyrion.invalid"]);
    fs::write(path.join("README.md"), "# Fixture\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-qm", "feat: seed fixture"]);
    git_output(path, &["rev-parse", "HEAD"]).trim().to_owned()
}

fn git(path: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(path: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn write_runtime_fixture(root: &Path, openshell: &Path, codex: &Path) -> PathBuf {
    let policy = root.join("hard-policy.yaml");
    fs::write(
        &policy,
        include_bytes!("../runtime/openshell/hard-landlock-policy.yaml"),
    )
    .unwrap();
    let gateway = root.join("gateway.toml");
    fs::write(
        &gateway,
        "[openshell.gateway]\ncompute_drivers = [\"vm\"]\n\n[openshell.gateway.mtls_auth]\nenabled = true\n\n[openshell.drivers.vm]\nvcpus = 2\nmem_mib = 2048\noverlay_disk_mib = 4096\n",
    )
    .unwrap();
    let kernel = root.join("kernel.config");
    fs::write(
        &kernel,
        "CONFIG_SECURITY=y\nCONFIG_SECURITY_LANDLOCK=y\nCONFIG_LSM=\"landlock,lockdown,yama,integrity\"\nCONFIG_CGROUP_PIDS=y\nCONFIG_SECCOMP_FILTER=y\n",
    )
    .unwrap();
    let runtime_artifact = root.join("libkrunfw.5.dylib");
    fs::write(&runtime_artifact, b"fixture runtime").unwrap();
    let config_home = root.join("openshell-config");
    fs::create_dir(&config_home).unwrap();
    let config = root.join("codex-worker.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "openshell_binary": openshell,
            "openshell_sha256": sha256_file(openshell),
            "openshell_version": "openshell 0.0.104",
            "openshell_config_home": config_home,
            "policy_path": policy,
            "policy_sha256": sha256_file(&policy),
            "gateway_config_path": gateway,
            "gateway_config_sha256": sha256_file(&gateway),
            "kernel_config_path": kernel,
            "kernel_config_sha256": sha256_file(&kernel),
            "runtime_artifacts": [{
                "path": runtime_artifact,
                "sha256": sha256_file(&runtime_artifact)
            }],
            "source_revision": "dd2b4e3bc0688bdd59f90030f7c1d52511d6e354",
            "source_patch_path": concat!(env!("CARGO_MANIFEST_DIR"), "/runtime/openshell/repaired-v0.0.104.patch"),
            "source_patch_sha256": "6452fbe2836ffbe43e0e73c813db5dc5dda7ee70537b7033fc5429573160e402",
            "base_image": "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e",
            "codex_binary": codex,
            "codex_version": "codex-cli 0.147.0",
            "codex_sha256": sha256_file(codex),
            "model": "fixture-model",
            "openshell_provider": "fixture-codex",
            "lease_ttl_seconds": 30,
            "vcpus": 2,
            "memory_mib": 2048,
            "overlay_disk_mib": 4096,
            "max_processes": 256
        }))
        .unwrap(),
    )
    .unwrap();
    config
}

fn add_claude_runtime_fixture(root: &Path, runtime: &Path) {
    let policy = root.join("hard-claude-policy.yaml");
    fs::write(
        &policy,
        include_bytes!("../runtime/openshell/hard-landlock-claude-policy.yaml"),
    )
    .unwrap();
    let claude = write_executable(
        &root.join("claude"),
        include_str!("fixtures/fake_claude.sh"),
    );
    let mut config: Value = serde_json::from_slice(&fs::read(runtime).unwrap()).unwrap();
    config["claude"] = json!({
        "policy_path": policy,
        "policy_sha256": sha256_file(&policy),
        "openshell_provider": "fixture-claude",
        "binary": claude,
        "version": "2.1.204 (Claude Code)",
        "sha256": sha256_file(&claude)
    });
    fs::write(runtime, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

fn write_executable(path: &Path, contents: &str) -> PathBuf {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    path.to_owned()
}

fn sha256_file(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
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

fn connect_full_entry(daemon: &RunningDaemon) -> String {
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
            "issue-git-token",
        ],
    );
    let connected = run_cli(
        &daemon.socket_path,
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
            "git-commission-session",
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
            "connect-git-session",
        ],
    );
    connected["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn create_and_accept(
    daemon: &RunningDaemon,
    attachment_token: &str,
    proposal_path: &Path,
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
            "create-failing-git-commission",
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
            "accept-failing-git-commission",
        ],
    );
    commission_id
}

fn wait_for_completion(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
) -> Value {
    wait_for_completion_with_timeout(
        daemon,
        attachment_token,
        commission_id,
        Duration::from_secs(10),
    )
}

fn wait_for_completion_with_timeout(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                attachment_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        if inspected["commission"]["status"] == "verified_complete" {
            return inspected;
        }
        assert!(
            !inspected["attempts"].as_array().is_some_and(|attempts| {
                attempts
                    .first()
                    .is_some_and(|attempt| attempt["status"] == "failed")
            }),
            "Attempt failed before Commission completion: {inspected}"
        );
        assert!(
            Instant::now() < deadline,
            "Commission did not complete: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_failed_attempt(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                attachment_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        if inspected["attempts"].as_array().is_some_and(|attempts| {
            attempts
                .first()
                .is_some_and(|attempt| attempt["status"] == "failed")
        }) {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Attempt did not fail: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_verification_failure(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                attachment_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        if inspected["assignments"]
            .as_array()
            .and_then(|assignments| assignments.first())
            .is_some_and(|assignment| assignment["status"] == "verification_failed")
        {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Verification did not fail: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_reconciliation(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                attachment_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        let reconciliation_event_exists = inspected["events"].as_array().is_some_and(|events| {
            events
                .iter()
                .any(|event| event["type"] == "reconciliation_required")
        });
        let reconciliation_assignment_exists =
            inspected["assignments"]
                .as_array()
                .is_some_and(|assignments| {
                    assignments
                        .iter()
                        .any(|assignment| assignment["purpose"] == "reconciliation")
                });
        if reconciliation_event_exists && reconciliation_assignment_exists {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Reconciliation was not created: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_frontier_hold(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    logical_id: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspected = run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                attachment_token,
                "commission",
                "inspect",
                commission_id,
            ],
        );
        if inspected["frontier_holds"].as_array().is_some_and(|holds| {
            holds.iter().any(|hold| {
                hold["logical_id"] == logical_id && hold["reason"] == "declared_write_overlap"
            })
        }) {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Assignment was not held from the frontier: {inspected}"
        );
        thread::sleep(Duration::from_millis(25));
    }
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

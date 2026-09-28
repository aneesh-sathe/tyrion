#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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

    /// Start with the Principal control credential delivered once over a
    /// private pipe, as `tyriond --principal-control-bootstrap-fd` does.
    fn start_with_principal(data_dir: &Path, worker_config: &Path) -> (Self, String) {
        use std::os::unix::io::FromRawFd;
        use std::os::unix::process::CommandExt;
        let socket_path = data_dir.join("tyrion.sock");
        let mut descriptors = [0_i32; 2];
        // SAFETY: pipe initializes both descriptors, which are closed exactly once below.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let read_fd = descriptors[0];
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command.args([
            "--data-dir",
            path_text(data_dir),
            "--socket",
            path_text(&socket_path),
            "--codex-worker-config",
            path_text(worker_config),
            "--principal-control-bootstrap-fd",
            &descriptors[1].to_string(),
        ]);
        // SAFETY: close is async-signal-safe and removes the read end from the child.
        unsafe {
            command.pre_exec(move || {
                if libc::close(read_fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let child = command.spawn().expect("daemon should start");
        // SAFETY: the child inherited the write end and the parent no longer needs it.
        assert_eq!(unsafe { libc::close(descriptors[1]) }, 0);
        // SAFETY: the parent owns the read end until the File drops it.
        let pipe = unsafe { fs::File::from_raw_fd(read_fd) };
        let mut line = String::new();
        BufReader::new(pipe).read_line(&mut line).unwrap();
        let principal = line
            .trim()
            .strip_prefix("TYRION_PRINCIPAL_CONTROL_TOKEN=")
            .expect("daemon should emit the Principal credential")
            .to_owned();
        let mut daemon = Self { child, socket_path };
        daemon.wait_until_ready();
        (daemon, principal)
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
#[ignore = "requires a provisioned Worker image and a real Codex credential"]
fn real_docker_boundary_completes_the_contained_git_assignment() {
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
#[ignore = "requires `tyrion init` output and a real Codex login"]
fn real_opencode_worker_completes_the_contained_git_assignment() {
    let runtime = std::env::var_os("TYRION_REAL_WORKER_RUNTIME")
        .map(PathBuf::from)
        .expect("set TYRION_REAL_WORKER_RUNTIME to the worker-runtime.json `tyrion init` wrote");
    let catalog = std::env::var_os("TYRION_REAL_WORKER_CATALOG")
        .map(PathBuf::from)
        .expect("set TYRION_REAL_WORKER_CATALOG to the worker-catalog.json `tyrion init` wrote");
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start_with_catalog(&data_dir, &runtime, &catalog);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["worker_requirements"] = json!({
        "tools": ["git"],
        "assignment_constraints": ["coding"],
        "require_configurations": ["opencode-default"]
    });
    proposal["resource_ceilings"]["max_elapsed_seconds"] = json!(900);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let completed = wait_for_completion_with_timeout(
        &daemon,
        &attachment_token,
        &commission_id,
        Duration::from_secs(900),
    );
    assert_eq!(completed["commission"]["status"], "verified_complete");
    let worker = &completed["workers"][0];
    assert_eq!(worker["configuration"]["id"], "opencode-default");
    assert!(worker["native_session_id"]
        .as_str()
        .is_some_and(|session| session.starts_with("ses_")));
    assert!(worker["usage"]["output_tokens"].as_u64().unwrap_or(0) > 0);
    assert_eq!(completed["attempts"][0]["lease"]["status"], "released");
    assert!(!principal_checkout.join("issue-4.txt").exists());
}

#[test]
fn contained_codex_receives_the_subscription_login() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    fs::write(fake_state.join("expect-codex-login"), "").unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
    let login = temp.path().join("codex-auth.json");
    fs::write(
        &login,
        serde_json::to_vec(&json!({
            "tokens": {
                "id_token": "fixture-id",
                "access_token": "fixture-access",
                "refresh_token": "fixture-refresh",
                "account_id": "fixture-account"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut config: Value = serde_json::from_slice(&fs::read(&runtime).unwrap()).unwrap();
    config["codex_auth_file"] = json!(login);
    fs::write(&runtime, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    // The login travelled over stdin, never on a Docker command line.
    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap();
    assert!(!log.contains("fixture-refresh"));
}

#[test]
fn a_coding_commission_performs_one_approved_local_effect_outside_the_checkout() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let effect_dir = temp.path().join("effect-target");
    fs::create_dir(&effect_dir).unwrap();
    fs::write(effect_dir.join("effect.txt"), "before\n").unwrap();
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let (daemon, principal) = RunningDaemon::start_with_principal(&data_dir, &runtime);
    let attachment_token = connect_full_entry(&daemon);

    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    // Hold the Worker long enough to approve and perform the effect.
    proposal["goal"] =
        json!("TYRION_FIXTURE_DELAY=8 Add issue-4.txt containing contained codex result.");
    proposal["authority"]["repositories"] = json!([principal_checkout, effect_dir]);
    proposal["authority"]["paths"] = json!(["issue-4.txt", "effect.txt"]);
    proposal["authority"]["actions"] = json!(["codex.git_change", "filesystem.write"]);
    proposal["authority"]["destinations"] = json!(["local"]);
    proposal["authority"]["effects"] = json!(["filesystem.write"]);

    // The Principal checkout is never an effect target, however it is named.
    let mut inside = proposal.clone();
    inside["authority"]["repositories"] =
        json!([principal_checkout, principal_checkout.join(".git")]);
    fs::write(&proposal_path, serde_json::to_vec_pretty(&inside).unwrap()).unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "proposal",
            "create",
        ])
        .args([
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "inside",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("outside the Principal checkout"));

    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let running = loop {
        let state = inspect_commission(&daemon, &attachment_token, &commission_id);
        if state["attempts"][0]["status"] == "running" {
            break state;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let attempt = &running["attempts"][0];
    let operation = json!({
        "assignment_id": attempt["assignment_id"],
        "attempt_id": attempt["id"],
        "worker_lease_id": attempt["lease"]["id"],
        "mandate_revision": 1,
        "plan_revision": 1,
        "operation": "filesystem.write",
        "repository": effect_dir,
        "target": "effect.txt",
        "parameters": {"content": "after\n"},
        "destination": "local",
        "effect": "filesystem.write",
        "consequences": ["Replace effect.txt outside the Principal checkout"],
        "limits": {"max_output_bytes": 1024, "max_duration_seconds": 5}
    });
    let operation_path = temp.path().join("operation.json");
    fs::write(
        &operation_path,
        serde_json::to_vec_pretty(&operation).unwrap(),
    )
    .unwrap();
    let gated = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "operation",
            "propose",
            &commission_id,
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "propose-effect",
        ],
    );
    let gate = &gated["approval_gates"][0];
    assert_eq!(gate["status"], "open");
    assert_eq!(
        fs::read_to_string(effect_dir.join("effect.txt")).unwrap(),
        "before\n"
    );
    run_principal_cli(
        &daemon.socket_path,
        &principal,
        &[
            "principal",
            "approve-gate",
            &commission_id,
            gate["id"].as_str().unwrap(),
            "--expected-operation-digest",
            gate["operation_digest"].as_str().unwrap(),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "approve-effect",
        ],
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "operation",
            "execute",
            &commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "execute-effect",
        ],
    );
    assert_eq!(
        fs::read_to_string(effect_dir.join("effect.txt")).unwrap(),
        "after\n"
    );

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["run_report"]["approval_gates"]["consumed"], 1);
    assert!(!principal_checkout.join("issue-4.txt").exists());
    assert!(!principal_checkout.join("effect.txt").exists());
}

#[test]
fn contained_codex_result_is_verified_integrated_and_verified_again() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
    assert_eq!(log.matches("run --detach --name").count(), 3);
    assert_eq!(log.matches("rm --force").count(), 3);
    // One preflight descendant per sandbox, plus the Worker's own spawned
    // descendant, each terminated with its container.
    assert_eq!(log.matches("descendant-terminated").count(), 4);
    // Every sandbox carries the whole hardened profile, and each ceiling is
    // set by the Docker daemon from outside the container. The fixture logs
    // shell-quoted arguments, so compare against an unescaped copy.
    let log_plain = log.replace('\\', "");
    for hardening in [
        "--read-only",
        "--pids-limit 256",
        "--memory 3072m --memory-swap 3072m",
        "--cpus 2 --cpuset-cpus 0,1",
        "--cap-drop ALL",
        "--security-opt no-new-privileges",
        "--security-opt seccomp=builtin",
        "--user 65534:65534",
        "/sandbox:rw,exec,nosuid,nodev,size=2048m,mode=1777",
        "--network none",
    ] {
        assert!(
            log_plain.contains(hardening),
            "sandbox created without {hardening}"
        );
    }
    assert!(log.contains("registry.invalid/tyrion-worker@sha256:"));
    // Adapters import native_skill from the pinned image. The fixture adapter
    // is a shell script, so only this assertion catches a sandbox that could
    // not satisfy a real Python adapter's imports.
    assert!(log.contains("--env PYTHONPATH=/opt/tyrion"));
    assert!(log.contains("tyrion-containment-probe"));
    assert!(log.contains("descendant-terminated"));
    // The harness runs from the Worker image; no Worker receives its own copy.
    // The positive control proves the upload pattern is what the log records.
    assert!(log.contains("/opt/tyrion/harness/codex --version"));
    assert!(
        log_plain.contains("cat > '/sandbox/base.bundle'"),
        "{log_plain}"
    );
    assert!(!log_plain.contains("cat > '/sandbox/codex'"), "{log_plain}");
    assert!(!log.lines().any(|line| {
        line.contains("exec --interactive") && line.contains(path_text(&principal_checkout))
    }));
}

#[test]
fn restart_restores_unacknowledged_integration_before_retry() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
            "containment_profile": "docker-hardened-v1",
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

    let exported = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "export-record",
            &commission_id,
        ],
    );
    assert_eq!(exported["format"], "tyrion.commission");
    assert_eq!(exported["version"], 1);
    assert_eq!(exported["record"]["commission"]["id"], commission_id);
    assert_eq!(
        exported["record"]["commission"]["status"],
        "verified_complete"
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "approval_gates".to_owned(),
            "conflicts".to_owned(),
            "context_transfer".to_owned(),
            "corrections".to_owned(),
            "cost".to_owned(),
            "failures".to_owned(),
            "planned_principal_controls".to_owned(),
            "reconciliation".to_owned(),
            "recovery_events".to_owned(),
            "timing".to_owned(),
            "unplanned_principal_interventions".to_owned(),
            "useful_concurrency".to_owned(),
        ])
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]["approval_gates"]["required"],
        0
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]["planned_principal_controls"]["total"],
        0
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]["unplanned_principal_interventions"]["total"],
        0
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]["context_transfer"]["manual_events"],
        0
    );
    assert_eq!(
        exported["record"]["briefing"]["run_report"]["failures"]["security_invariant_failures"],
        0
    );
    assert!(
        exported["record"]["briefing"]["run_report"]["useful_concurrency"]["occurred"]
            .as_bool()
            .unwrap()
    );
    assert!(exported["summary_markdown"]
        .as_str()
        .unwrap()
        .contains("fixture-backed Worker evidence is not production containment attestation"));
    assert_eq!(exported["dogfood_readiness"]["status"], "blocked");
    assert!(exported["dogfood_readiness"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker["code"] == "fixture_backed_evidence"));
    let expected_checksum = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&exported["record"]).unwrap())
    );
    assert_eq!(exported["checksum"], expected_checksum);
    if let Some(record_path) = std::env::var_os("TYRION_CAPTURE_COMMISSION_RECORD") {
        fs::write(record_path, serde_json::to_vec_pretty(&exported).unwrap()).unwrap();
    }
}

#[test]
fn disjoint_useful_assignments_run_concurrently_and_complete_the_assembled_artifact() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
    // Issue #20: a clean run records no failure. A criterion is checked only
    // once its own work is in the assembled result.
    let failed = completed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["outcome"] != "passed")
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "spurious failed Evidence: {failed:?}");
    assert_eq!(
        completed["briefing"]["run_report"]["failures"]["failed_evidence"],
        0
    );
}

/// A disposable fixture runtime plus a daemon started with extra arguments.
fn fixture_daemon(temp: &TempDir, extra_arguments: &[&str]) -> (RunningDaemon, PathBuf) {
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &runtime, extra_arguments);
    (daemon, fake_state)
}

fn inspect_commission(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
) -> Value {
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

#[test]
fn concurrent_workers_are_pinned_to_disjoint_cpus_and_capacity_is_inspectable() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, fake_state) = fixture_daemon(&temp, &[]);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);

    // The fixture engine reports 16 CPUs and 32 GiB; 1 GiB is kept back.
    let host = &completed["host_capacity"];
    assert_eq!(host["source"], "container_runtime");
    assert_eq!(host["cpus"], 16);
    assert_eq!(host["usable_memory_mib"], 32768 - 1024);
    assert_eq!(host["default_worker_request"]["memory_mib"], 320);
    // min(16000 / 250 millicores, 31744 / 320 MiB) Workers at the pinned request.
    assert_eq!(host["derived_worker_ceiling"], 64);
    assert_eq!(
        host["reserved"]["memory_mib"], 0,
        "finished Workers hold nothing"
    );

    // The two Workers overlapped, so the second was given its own CPUs
    // rather than contending for the first two.
    let log = fs::read_to_string(fake_state.join("commands.log"))
        .unwrap()
        .replace('\\', "");
    assert!(log.contains("--cpuset-cpus 0,1"), "{log}");
    assert!(log.contains("--cpuset-cpus 2,3"), "{log}");
    assert!(
        completed["activity_journal"]["useful_concurrency"]["overlap_millis"]
            .as_u64()
            .is_some_and(|overlap| overlap > 0)
    );
}

#[test]
fn host_capacity_holds_workers_the_machine_cannot_run_together() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    // The Commission allows two concurrent Workers, but the declared host has
    // room for exactly one: 1500 MiB less the 1024 MiB reserve holds one
    // 320 MiB request, not two.
    let (daemon, _fake_state) = fixture_daemon(&temp, &["--host-memory-mib", "1500"]);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let deadline = Instant::now() + Duration::from_secs(45);
    let held = loop {
        let inspected = inspect_commission(&daemon, &attachment_token, &commission_id);
        let hold = inspected["frontier_holds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hold| hold["reason"] == "host_capacity_unavailable")
            .cloned();
        let running = inspected["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|attempt| attempt["status"] == "running");
        if let Some(hold) = hold.filter(|_| running) {
            break (inspected, hold);
        }
        assert_ne!(
            inspected["commission"]["status"], "verified_complete",
            "completed without ever holding for host capacity: {inspected}"
        );
        assert!(
            Instant::now() < deadline,
            "no host hold observed: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let (inspected, hold) = held;
    assert_eq!(inspected["host_capacity"]["source"], "principal");
    assert_eq!(inspected["host_capacity"]["derived_worker_ceiling"], 1);
    assert_eq!(inspected["host_capacity"]["reserved"]["memory_mib"], 320);
    let detail = hold["detail"].as_str().unwrap();
    assert!(
        detail.contains("Expected to use 0.25 CPUs and 320 MiB")
            && detail.contains("476 MiB for Workers"),
        "{detail}"
    );

    // Held work is dispatched, not dropped, once the first Worker finishes,
    // and the two never ran at the same time.
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(
        completed["activity_journal"]["useful_concurrency"]["overlap_millis"],
        0
    );
}

#[test]
fn a_worker_profile_the_host_can_never_run_blocks_with_the_exact_requirement() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    // 1300 MiB less the 1024 MiB reserve cannot hold one 320 MiB request.
    let (daemon, fake_state) = fixture_daemon(&temp, &["--host-memory-mib", "1300"]);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let deadline = Instant::now() + Duration::from_secs(20);
    let blocker = loop {
        let inspected = inspect_commission(&daemon, &attachment_token, &commission_id);
        if let Some(blocker) = inspected["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|blocker| blocker["code"] == "host_capacity")
        {
            assert_eq!(inspected["assignments"][0]["status"], "resource_blocked");
            assert!(inspected["attempts"].as_array().unwrap().is_empty());
            break blocker.clone();
        }
        assert!(Instant::now() < deadline, "no host blocker: {inspected}");
        thread::sleep(Duration::from_millis(20));
    };
    let requirement = blocker["requirement"].as_str().unwrap();
    assert!(
        requirement.contains("expected to use 0.25 CPUs and 320 MiB"),
        "{requirement}"
    );
    assert!(requirement.contains("276 MiB for Workers"), "{requirement}");
    assert!(requirement.contains("--host-memory-mib"), "{requirement}");
    // Nothing was started only to be killed.
    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap_or_default();
    assert!(!log.contains("run --detach"), "{log}");
}

/// One JSON-RPC exchange with an Entry MCP server, as a native harness does it.
fn mcp_call(input: &mut impl Write, output: &mut impl BufRead, request: Value) -> Value {
    writeln!(input, "{request}").unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn an_entry_session_runs_a_parallel_plan_concurrently() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, _fake_state) = fixture_daemon(&temp, &[]);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();

    // The same process Claude Code or Codex holds open for a session.
    let mut entry = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args([
            "entry-mcp",
            "--socket",
            path_text(&daemon.socket_path),
            "--harness",
            "codex",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Entry MCP server should launch");
    let mut input = entry.stdin.take().unwrap();
    let mut output = BufReader::new(entry.stdout.take().unwrap());
    mcp_call(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "plan-test", "version": "1.0.0"}
            }
        }),
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();

    let started = mcp_call(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "tyrion_start_commission", "arguments": {"proposal": proposal}}
        }),
    );
    assert_eq!(started["result"]["isError"], false, "{started}");

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut id = 3;
    loop {
        let status = mcp_call(
            &mut input,
            &mut output,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": "tyrion_status", "arguments": {}}
            }),
        );
        id += 1;
        let digest = status["result"]["structuredContent"].clone();
        // A host polls the digest: small, and it never carries Evidence.
        assert!(
            digest["evidence"].is_null(),
            "status returned the full record"
        );
        if digest["commission"]["status"] == "verified_complete" {
            assert_eq!(
                digest["headline"],
                "2 of 2 done, 0 running, 0 queued, 0 held, 0 blocked; 0¢ reported"
            );
            assert!(digest["needs_you"].as_array().unwrap().is_empty());
            assert!(digest["review"]["commands"].is_array());
            assert!(digest["parallel_speedup"]["serial_s"].is_u64());
            break;
        }
        assert!(Instant::now() < deadline, "plan did not complete: {digest}");
        thread::sleep(Duration::from_millis(50));
    }
    // The full record is one request away.
    let detailed = mcp_call(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "tyrion_status", "arguments": {"detail": true}}
        }),
    );
    let completed = detailed["result"]["structuredContent"].clone();
    assert!(completed["evidence"].is_array());

    assert_eq!(completed["assignments"].as_array().unwrap().len(), 2);
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["status"] == "accepted"));
    let concurrency = &completed["activity_journal"]["useful_concurrency"];
    assert!(
        concurrency["overlap_millis"].as_u64().unwrap() > 0,
        "the Workers did not run at the same time: {concurrency}"
    );
    assert!(
        concurrency["elapsed_time_reduction_millis"]
            .as_u64()
            .unwrap()
            > 0,
        "running in parallel saved no time: {concurrency}"
    );

    // The host is told where the work is and how to take it, and following
    // those commands from the Principal's checkout really does fetch it.
    let review = &completed["review"];
    assert_eq!(review["verified_complete"], true);
    assert_eq!(review["principal_checkout_changed"], false);
    assert!(!principal_checkout.join("backend.txt").exists());
    let fetch = review["commands"][0].as_str().unwrap();
    let fetched = Command::new("sh")
        .args(["-c", fetch])
        .current_dir(&principal_checkout)
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    let files = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "FETCH_HEAD"])
        .current_dir(&principal_checkout)
        .output()
        .unwrap();
    let files = String::from_utf8_lossy(&files.stdout);
    assert!(
        files.contains("backend.txt") && files.contains("frontend.txt"),
        "{files}"
    );

    drop(input);
    assert!(entry.wait().unwrap().success());
}

#[test]
fn an_entry_session_still_refuses_competition_and_external_effects() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, fake_state) = fixture_daemon(&temp, &[]);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();

    let mut competing = proposal.clone();
    for assignment in competing["plan"]["assignments"].as_array_mut().unwrap() {
        assignment["competition"] = json!({
            "group": "g", "uncertainty": "which is better", "comparison_rule": "fewest lines"
        });
    }
    let mut external = proposal.clone();
    external["authority"]["destinations"] = json!(["https://api.example.com"]);
    let mut judged = proposal;
    judged["criteria"][0]["verifier_type"] = json!("model");

    let mut entry = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args([
            "entry-mcp",
            "--socket",
            path_text(&daemon.socket_path),
            "--harness",
            "claude",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Entry MCP server should launch");
    let mut input = entry.stdin.take().unwrap();
    let mut output = BufReader::new(entry.stdout.take().unwrap());
    mcp_call(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "refusal-test", "version": "1.0.0"}}
        }),
    );
    for (id, (proposal, reason)) in [
        (competing, "competing Attempts"),
        (external, "external destinations"),
        (judged, "deterministic verifiers"),
    ]
    .into_iter()
    .enumerate()
    {
        let refused = mcp_call(
            &mut input,
            &mut output,
            json!({
                "jsonrpc": "2.0", "id": id + 2, "method": "tools/call",
                "params": {"name": "tyrion_start_commission", "arguments": {"proposal": proposal}}
            }),
        );
        let text = refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert!(text.contains(reason), "{text}");
    }
    // Nothing was started for any refused proposal.
    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap_or_default();
    assert!(!log.contains("run --detach"), "{log}");
    drop(input);
    assert!(entry.wait().unwrap().success());
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
            // Tyrion declares no spend ceiling it cannot enforce.
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0,
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
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    fs::write(fake_state.join("fail-preflight"), b"").unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
    assert_eq!(failed["blockers"][0]["code"], "security_invariant_failure");
    assert!(failed["blockers"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("simulated containment failure"));
    assert_eq!(
        failed["run_report"]["failures"]["security_invariant_failures"],
        1
    );
    let exported = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "export-record",
            &commission_id,
        ],
    );
    assert_eq!(exported["dogfood_readiness"]["status"], "blocked");
    assert!(exported["dogfood_readiness"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker["code"] == "security_invariant_failures"));
    assert!(!data_dir.join("integrations").exists());
    assert!(!principal_checkout.join("issue-4.txt").exists());

    let log = fs::read_to_string(fake_state.join("commands.log")).unwrap();
    assert_eq!(log.matches("run --detach --name").count(), 1);
    assert_eq!(log.matches("rm --force").count(), 1);
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
    assert!(log.contains("/opt/tyrion/harness/codex --version"));
    assert!(!log.contains("run-attempt.sh"));
}

#[test]
fn malformed_returned_bundle_never_reaches_verification_or_integration() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    fs::write(fake_state.join("corrupt-candidate"), b"").unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
    assert_eq!(log.matches("run --detach --name").count(), 1);
    assert_eq!(log.matches("rm --force").count(), 1);
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
fn a_verifier_that_cannot_run_blocks_once_instead_of_rerunning_the_worker() {
    // A host model wrote `python`, which the Worker image lacked. Rerunning the
    // Worker can never make a missing verifier appear, so it must block once,
    // keep the Worker's Result, and say exactly what is wrong.
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, _fake_state) = fixture_daemon(&temp, &[]);
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["criteria"][0]["verifier"]["argv"] = json!(["tyrion-missing-verifier", "--check"]);
    proposal["resource_ceilings"]["max_attempts"] = json!(3);
    fs::write(&proposal_path, serde_json::to_vec(&proposal).unwrap()).unwrap();
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let deadline = Instant::now() + Duration::from_secs(20);
    let blocked = loop {
        let inspected = inspect_commission(&daemon, &attachment_token, &commission_id);
        if inspected["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker["code"] == "verifier_unrunnable")
        {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "no verifier blocker: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    // Settle, then prove no second Attempt was spent.
    thread::sleep(Duration::from_millis(500));
    let settled = inspect_commission(&daemon, &attachment_token, &commission_id);
    assert_eq!(
        settled["attempts"].as_array().unwrap().len(),
        1,
        "{settled}"
    );
    assert_eq!(settled["results"][0]["status"], "candidate");
    let requirement = blocked["blockers"][0]["requirement"].as_str().unwrap();
    assert!(
        requirement.contains("verifier executable unavailable: tyrion-missing-verifier"),
        "{requirement}"
    );
    assert!(requirement.contains("Result is retained"), "{requirement}");
}

#[test]
fn workers_that_commit_their_own_work_integrate_without_reconciliation() {
    // Two parallel writers each commit their own change and then leave an
    // empty commit. The second integrates by cherry-pick, where git reports
    // an empty commit exactly like a conflict.
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, fake_state) = fixture_daemon(&temp, &[]);
    fs::write(fake_state.join("worker-commits-itself"), "").unwrap();
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("parallel-proposal.json");
    write_parallel_git_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    let assignments = completed["assignments"].as_array().unwrap();
    assert_eq!(
        assignments.len(),
        2,
        "no reconciliation was opened: {assignments:?}"
    );
    assert!(completed["blockers"].as_array().unwrap().is_empty());
}

/// The parallel proposal with its plan removed: Tyrion's planning Worker is
/// asked for the decomposition instead.
fn write_planning_proposal(path: &Path, principal_checkout: &Path, base_revision: &str) {
    write_parallel_git_proposal(path, principal_checkout, base_revision);
    let mut proposal: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    proposal.as_object_mut().unwrap().remove("plan");
    proposal["planning"] = json!("worker");
    proposal["resource_ceilings"]["max_attempts"] = json!(5);
    proposal["resource_ceilings"]["max_elapsed_seconds"] = json!(60);
    fs::write(path, serde_json::to_vec_pretty(&proposal).unwrap()).unwrap();
}

fn place_planner_output(fake_state: &Path, round: u32, assignments: Value) {
    fs::write(
        fake_state.join(format!("plan-{round}.json")),
        json!({"assignments": assignments}).to_string(),
    )
    .unwrap();
}

#[test]
fn a_planning_worker_decomposes_the_goal_and_its_plan_runs_in_parallel() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, fake_state) = fixture_daemon(&temp, &[]);
    place_planner_output(
        &fake_state,
        1,
        json!([
            {"id": "backend", "goal": "TYRION_FIXTURE_WRITE=backend.txt TYRION_FIXTURE_CONTENT=backend TYRION_FIXTURE_DELAY=1",
             "criterion_ids": ["backend-file"], "write_scopes": ["backend.txt"],
             "purpose": "ignored: Tyrion sets it"},
            {"id": "frontend", "goal": "TYRION_FIXTURE_WRITE=frontend.txt TYRION_FIXTURE_CONTENT=frontend TYRION_FIXTURE_DELAY=1",
             "criterion_ids": ["frontend-file"], "write_scopes": ["frontend.txt"]}
        ]),
    );
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("planning-proposal.json");
    write_planning_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);

    assert_eq!(completed["commission"]["status"], "verified_complete");
    // The planner was told the mandate it had to plan within.
    let prompt = fs::read_to_string(fake_state.join("planning-prompt-1.txt")).unwrap();
    for expected in [
        "backend-file",
        "frontend-file",
        "- backend.txt",
        "- frontend.txt",
    ] {
        assert!(
            prompt.contains(expected),
            "planning prompt lacks {expected}"
        );
    }
    // Revision 1 is the planning step; revision 2 is the Worker's validated
    // plan. Integration records further revisions as work lands.
    let plans = completed["plans"].as_array().unwrap();
    assert!(plans[0]["reason"]
        .as_str()
        .unwrap()
        .contains("planning Worker will propose"));
    assert_eq!(plans[1]["revision"], 2);
    assert!(plans[1]["reason"]
        .as_str()
        .unwrap()
        .contains("proposed 2 Assignments; the Control Plane validated them"));
    assert!(completed["events"].as_array().unwrap().iter().any(|event| {
        event["type"] == "plan_revised" && event["payload"]["source"] == "planning_worker"
    }));
    let logical: Vec<&str> = completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|assignment| assignment["logical_id"].as_str().unwrap())
        .collect();
    assert_eq!(logical, ["tyrion-plan-1", "backend", "frontend"]);
    // The planned work ran in parallel, and the comparison with serial
    // execution is recorded either way.
    let concurrency = &completed["activity_journal"]["useful_concurrency"];
    assert!(concurrency["serial_execution_millis"].as_u64().unwrap() > 0);
    assert!(
        concurrency["overlap_millis"].as_u64().unwrap() > 0,
        "{concurrency}"
    );
}

#[test]
fn a_rejected_plan_is_proposed_again_with_the_reason_and_then_blocks() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let (daemon, fake_state) = fixture_daemon(&temp, &[]);
    // First: authority outside the accepted envelope.
    place_planner_output(
        &fake_state,
        1,
        json!([
            {"id": "backend", "goal": "g", "criterion_ids": ["backend-file"], "write_scopes": ["outside.txt"]},
            {"id": "frontend", "goal": "g", "criterion_ids": ["frontend-file"], "write_scopes": ["frontend.txt"]}
        ]),
    );
    // Then: two writers of one file with no order between them.
    place_planner_output(
        &fake_state,
        2,
        json!([
            {"id": "backend", "goal": "g", "criterion_ids": ["backend-file"], "write_scopes": ["backend.txt"]},
            {"id": "frontend", "goal": "g", "criterion_ids": ["frontend-file"], "write_scopes": ["backend.txt"]}
        ]),
    );
    let attachment_token = connect_full_entry(&daemon);
    let proposal_path = temp.path().join("planning-proposal.json");
    write_planning_proposal(&proposal_path, &principal_checkout, &base_revision);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);

    let deadline = Instant::now() + Duration::from_secs(30);
    let blocked = loop {
        let inspected = inspect_commission(&daemon, &attachment_token, &commission_id);
        if inspected["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker["code"] == "planning_failed")
        {
            break inspected;
        }
        assert!(
            Instant::now() < deadline,
            "no planning blocker: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    // The second round was told exactly why the first was rejected.
    let second = fs::read_to_string(fake_state.join("planning-prompt-2.txt")).unwrap();
    assert!(second.contains("previous plan was rejected"), "{second}");
    assert!(
        second.contains("unauthorized write scope outside.txt"),
        "{second}"
    );
    let requirement = blocked["blockers"][0]["requirement"].as_str().unwrap();
    assert!(
        requirement.contains("both write backend.txt but neither depends on the other"),
        "{requirement}"
    );
    // Only the planners ran: nothing that writes was dispatched.
    assert_eq!(blocked["attempts"].as_array().unwrap().len(), 2);
    assert!(!principal_checkout.join("outside.txt").exists());

    // Planning failure does not wedge the Commission.
    let revision = blocked["commission"]["revision"]
        .as_i64()
        .unwrap()
        .to_string();
    let cancelled = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "cancel",
            &commission_id,
            "--expected-revision",
            &revision,
            "--idempotency-key",
            "cancel-after-planning-failure",
        ],
    );
    assert_eq!(cancelled["commission"]["status"], "cancelled");
}

#[test]
fn test_run_byproducts_do_not_fail_a_correct_worker() {
    // The repository has no .gitignore, and the Worker ran its tests. Before
    // this, staging swept the interpreter's caches into the Result and the
    // correct work was rejected as an unauthorized change.
    let fixture = FailedFixture::new("runtime-byproducts");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");
    assert_eq!(completed["blockers"], json!([]));
    let changed = completed["results"][0]["changed_paths"].to_string();
    assert!(changed.contains("issue-4.txt"), "{changed}");
    for byproduct in ["__pycache__", "node_modules", ".pytest_cache"] {
        assert!(
            !changed.contains(byproduct),
            "{byproduct} leaked: {changed}"
        );
    }
}

/// A byte-level fingerprint of the Principal checkout: every path, its mode,
/// and its content or symlink target. Used to prove a Commission leaves the
/// Principal's own directory exactly as it found it.
fn checkout_fingerprint(root: &Path) -> Vec<String> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<String>) {
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let mode = metadata.permissions().mode();
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.push(format!("{relative} symlink {mode:o} {}", target.display()));
            } else if metadata.is_dir() {
                out.push(format!("{relative} dir {mode:o}"));
                walk(root, &path, out);
            } else {
                let digest = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
                out.push(format!("{relative} file {mode:o} {digest}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// The Principal's directory is an input, never a workspace. A Worker runs in
/// a container that cannot see it, and Integration targets a daemon-owned
/// repository, so a completed Commission must leave it byte-identical.
#[test]
fn a_completed_commission_leaves_the_principal_checkout_byte_identical() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let principal_checkout = temp.path().join("principal-checkout");
    let base_revision = create_principal_repository(&principal_checkout);
    let fake_state = temp.path().join("fake-docker");
    fs::create_dir(&fake_state).unwrap();
    let fake_docker = write_executable(
        &temp.path().join("docker"),
        include_str!("fixtures/fake_docker.sh"),
    );
    let fake_codex = write_executable(
        &temp.path().join("codex"),
        include_str!("fixtures/fake_codex.sh"),
    );
    let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let proposal_path = temp.path().join("proposal.json");
    write_git_proposal(&proposal_path, &principal_checkout, &base_revision);

    let before = checkout_fingerprint(&principal_checkout);
    assert!(
        !before.is_empty(),
        "fingerprint should observe real content"
    );

    let daemon = RunningDaemon::start(&data_dir, &runtime, &fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &proposal_path);
    let completed = wait_for_completion(&daemon, &attachment_token, &commission_id);
    assert_eq!(completed["commission"]["status"], "verified_complete");

    let after = checkout_fingerprint(&principal_checkout);
    assert_eq!(
        before, after,
        "a completed Commission modified the Principal checkout"
    );
    // The accepted artifact exists, but only inside Tyrion's own repository.
    assert!(!principal_checkout.join("issue-4.txt").exists());
    assert!(data_dir
        .join("integrations")
        .join(&commission_id)
        .join("repository")
        .join("issue-4.txt")
        .exists());
    // Nothing the daemon wrote escaped its own data directory.
    let head = Command::new("git")
        .args(["-C", path_text(&principal_checkout), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        base_revision,
        "the Principal checkout moved off its base revision"
    );
}

/// A symlink is a path a Worker may be authorized to write, aimed at a host
/// file it is not authorized to read. It must be rejected for being a symlink
/// out of the repository, not merely caught later by whichever criterion
/// happens to read that path.
#[test]
fn a_symlink_aimed_at_the_host_is_rejected_before_verification() {
    let fixture = FailedFixture::new("symlink-escape");
    let daemon = RunningDaemon::start(&fixture.data_dir, &fixture.runtime, &fixture.fake_state);
    let attachment_token = connect_full_entry(&daemon);
    let commission_id = create_and_accept(&daemon, &attachment_token, &fixture.proposal_path);

    let failed = wait_for_failed_attempt(&daemon, &attachment_token, &commission_id);
    assert_eq!(failed["results"], json!([]));
    assert!(
        failed["blockers"][0]["requirement"]
            .as_str()
            .unwrap()
            .contains("points outside the repository"),
        "expected an explicit symlink rejection, got {}",
        failed["blockers"][0]["requirement"]
    );
    // Rejected before anything was integrated or verified.
    assert!(!fixture.data_dir.join("integrations").exists());
    assert!(!fixture.principal_checkout.join("outside.txt").exists());
    let failed = failed.to_string();
    assert!(
        !failed.contains("BEGIN OPENSSH PRIVATE KEY") && !failed.contains("BEGIN RSA"),
        "Result exposed host key material"
    );
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
    assert!(log.contains("rm --force"));
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
    let remaining_sandboxes = fs::read_dir(fixture.fake_state.join("containers"))
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
        let fake_state = temp.path().join("fake-docker");
        fs::create_dir(&fake_state).unwrap();
        let fake_docker = write_executable(
            &temp.path().join("docker"),
            include_str!("fixtures/fake_docker.sh"),
        );
        let fake_codex = write_executable(
            &temp.path().join("codex"),
            include_str!("fixtures/fake_codex.sh"),
        );
        let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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
        let fake_state = temp.path().join("fake-docker");
        fs::create_dir(&fake_state).unwrap();
        fs::write(fake_state.join(marker), b"").unwrap();
        let fake_docker = write_executable(
            &temp.path().join("docker"),
            include_str!("fixtures/fake_docker.sh"),
        );
        let fake_codex = write_executable(
            &temp.path().join("codex"),
            include_str!("fixtures/fake_codex.sh"),
        );
        let runtime = write_runtime_fixture(temp.path(), &fake_docker, &fake_codex);
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

fn write_runtime_fixture(root: &Path, docker: &Path, codex: &Path) -> PathBuf {
    install_in_image(root, "codex", codex);
    let config = root.join("codex-worker.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "docker_binary": docker,
            "docker_sha256": sha256_file(docker),
            "docker_version": "Docker version 28.0.4, build fixture",
            "docker_host": "unix:///fixture/docker.sock",
            "worker_image": format!("registry.invalid/tyrion-worker@sha256:{}", "b".repeat(64)),
            "worker_image_id": format!("sha256:{}", "1".repeat(64)),
            "codex_version": "codex-cli 0.156.1",
            "model": "fixture-model",
            "lease_ttl_seconds": 30,
            "memory_request_mib": 320,
            "cpu_request_millis": 250,
            "vcpus": 2,
            "memory_mib": 3072,
            "writable_storage_mib": 2048,
            "max_processes": 256
        }))
        .unwrap(),
    )
    .unwrap();
    config
}

fn add_claude_runtime_fixture(root: &Path, runtime: &Path) {
    let claude = write_executable(
        &root.join("claude"),
        include_str!("fixtures/fake_claude.sh"),
    );
    install_in_image(root, "claude", &claude);
    let mut config: Value = serde_json::from_slice(&fs::read(runtime).unwrap()).unwrap();
    config["claude"] = json!({"version": "2.1.204 (Claude Code)"});
    fs::write(runtime, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
}

/// Put a harness binary where the Worker image carries it: the fake Docker
/// runs `/opt/tyrion/harness/<name>` from this directory.
fn install_in_image(root: &Path, name: &str, binary: &Path) {
    let harness = root.join("fake-docker").join("image").join("harness");
    fs::create_dir_all(&harness).unwrap();
    fs::copy(binary, harness.join(name)).unwrap();
    fs::set_permissions(harness.join(name), fs::Permissions::from_mode(0o700)).unwrap();
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
        Duration::from_secs(45),
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
        let attempt_failed = inspected["attempts"].as_array().is_some_and(|attempts| {
            attempts
                .first()
                .is_some_and(|attempt| attempt["status"] == "failed")
        });
        let failure_projected = inspected["assignments"]
            .as_array()
            .and_then(|assignments| assignments.first())
            .is_some_and(|assignment| assignment["status"] != "running")
            && inspected["blockers"]
                .as_array()
                .is_some_and(|blockers| !blockers.is_empty());
        if attempt_failed && failure_projected {
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
    let deadline = Instant::now() + Duration::from_secs(45);
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
    let deadline = Instant::now() + Duration::from_secs(45);
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

fn run_principal_cli(socket_path: &Path, principal: &str, arguments: &[&str]) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args([
            "--socket",
            path_text(socket_path),
            "--principal-token-stdin",
        ])
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Principal CLI should run");
    writeln!(child.stdin.as_mut().unwrap(), "{principal}").unwrap();
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

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

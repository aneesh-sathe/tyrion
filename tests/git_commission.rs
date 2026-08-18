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

impl RunningDaemon {
    fn start(data_dir: &Path, worker_config: &Path, _fake_state: &Path) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_tyriond"))
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(&socket_path),
                "--codex-worker-config",
                path_text(worker_config),
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
        .unwrap()
        .contains("codex-cli 0.147.0"));

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

struct FailedFixture {
    _temp: TempDir,
    principal_checkout: PathBuf,
    fake_state: PathBuf,
    runtime: PathBuf,
    data_dir: PathBuf,
    proposal_path: PathBuf,
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

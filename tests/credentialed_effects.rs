#![cfg(all(unix, target_os = "macos"))]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const KEYCHAIN_PASSWORD: &str = "tyrion-test-keychain";
const KEYCHAIN_SERVICE: &str = "dev.tyrion.credential-effects";

struct RunningDaemon {
    child: Child,
    data_dir: PathBuf,
    socket_path: PathBuf,
    credential_runtime: PathBuf,
    principal_token: String,
}

impl RunningDaemon {
    fn start(data_dir: &Path, credential_runtime: &Path) -> Self {
        Self::start_with_arguments(data_dir, credential_runtime, &[])
    }

    fn start_with_arguments(
        data_dir: &Path,
        credential_runtime: &Path,
        arguments: &[&str],
    ) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let (child, principal_token) =
            Self::spawn(data_dir, credential_runtime, &socket_path, arguments);
        let mut daemon = Self {
            child,
            data_dir: data_dir.to_owned(),
            socket_path,
            credential_runtime: credential_runtime.to_owned(),
            principal_token,
        };
        daemon.wait_until_ready();
        daemon
    }

    fn spawn(
        data_dir: &Path,
        credential_runtime: &Path,
        socket_path: &Path,
        arguments: &[&str],
    ) -> (Child, String) {
        let mut descriptors = [0_i32; 2];
        // SAFETY: pipe initializes both descriptors, which are closed exactly once below.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let bootstrap_fd = descriptors[1].to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(socket_path),
                "--fault-hold-worker-for-control",
                "--principal-control-bootstrap-fd",
                &bootstrap_fd,
                "--credential-runtime",
                path_text(credential_runtime),
            ])
            .args(arguments);
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
        let (child, principal_token) = Self::spawn(
            &self.data_dir,
            &self.credential_runtime,
            &self.socket_path,
            &[],
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
fn brokered_effect_uses_a_single_keychain_credential_without_projecting_it() {
    let temp = TempDir::new().unwrap();
    let keychain = temp.path().join("credentials.keychain-db");
    create_keychain(&keychain);
    let credential_reference = "brokered-release-token";
    let secret = "tyrion-brokered-secret-83c6d93e";
    add_keychain_credential(&keychain, credential_reference, secret);

    let (destination, received) = start_http_effect_server();
    let runtime_path = temp.path().join("credential-runtime.json");
    fs::write(
        &runtime_path,
        serde_json::to_vec_pretty(&json!({
            "keychain": {
                "security_binary": "/usr/bin/security",
                "security_sha256": sha256_file(Path::new("/usr/bin/security")),
                "keychain_path": path_text(&keychain),
                "service": KEYCHAIN_SERVICE
            },
            "broker": {
                "curl_binary": "/usr/bin/curl",
                "curl_sha256": sha256_file(Path::new("/usr/bin/curl")),
                "destinations": {"dogfood-api": destination}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let daemon = RunningDaemon::start(temp.path(), &runtime_path);
    let attachment_token = connect_full_entry(&daemon, "brokered-effect");
    let proposal_path = temp.path().join("proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal()).unwrap(),
    )
    .unwrap();
    let created = run_cli(
        &daemon,
        &attachment_token,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-brokered-effect",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    run_cli(
        &daemon,
        &attachment_token,
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-brokered-effect",
        ],
    );
    let running = wait_for(&daemon, &attachment_token, commission_id, |state| {
        state["attempts"].as_array().is_some_and(|attempts| {
            attempts
                .iter()
                .any(|attempt| attempt["status"] == "running")
        })
    });
    let attempt = running["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attempt| attempt["status"] == "running")
        .unwrap();
    let grant_path = temp.path().join("credential-grant.json");
    fs::write(
        &grant_path,
        serde_json::to_vec_pretty(&json!({
            "assignment_id": attempt["assignment_id"],
            "attempt_id": attempt["id"],
            "worker_lease_id": attempt["lease"]["id"],
            "mandate_revision": 1,
            "plan_revision": 1,
            "credential_reference": credential_reference,
            "capability": "http_bearer",
            "destination": "dogfood-api",
            "exposure": "brokered_only",
            "credential_expires_at": unix_timestamp() + 300,
            "revocation": "delete_from_keychain"
        }))
        .unwrap(),
    )
    .unwrap();
    let granted = run_principal_cli(
        &daemon,
        &[
            "principal",
            "grant-credential",
            commission_id,
            "--file",
            path_text(&grant_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "grant-brokered-credential",
        ],
    );
    let grant_id = granted["credential_grants"][0]["id"].as_str().unwrap();
    let operation = json!({
        "assignment_id": attempt["assignment_id"],
        "attempt_id": attempt["id"],
        "worker_lease_id": attempt["lease"]["id"],
        "mandate_revision": 1,
        "plan_revision": 1,
        "operation": "credential.http.request",
        "repository": null,
        "target": "effects/brokered",
        "parameters": {
            "body": "{\"release\":\"v1\"}",
            "content_type": "application/json",
            "method": "POST",
            "reconciliation_target": "effects/brokered",
            "confirmed_reconciliation_sha256": format!("{:x}", Sha256::digest(b"applied")),
            "not_applied_reconciliation_sha256": format!("{:x}", Sha256::digest(b"not-applied"))
        },
        "destination": "dogfood-api",
        "effect": "external.write",
        "credential": {"grant_id": grant_id, "mode": "brokered"},
        "consequences": ["Create the exact dogfood release marker"],
        "limits": {
            "max_output_bytes": 1024,
            "max_duration_seconds": 5,
            "max_paid_service_spend_cents": 0
        }
    });
    let operation_path = temp.path().join("brokered-operation.json");
    fs::write(
        &operation_path,
        serde_json::to_vec_pretty(&operation).unwrap(),
    )
    .unwrap();
    let proposed = run_cli(
        &daemon,
        &attachment_token,
        &[
            "operation",
            "propose",
            commission_id,
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "propose-brokered-operation",
        ],
    );
    assert_eq!(
        proposed["operation_requests"][0]["classification"],
        "approval_gate"
    );
    let gate = &proposed["approval_gates"][0];
    run_principal_cli(
        &daemon,
        &[
            "principal",
            "approve-gate",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--expected-operation-digest",
            gate["operation_digest"].as_str().unwrap(),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "approve-brokered-operation",
        ],
    );
    let mut changed_operation = operation.clone();
    changed_operation["credential"]["grant_id"] = json!("forged-grant");
    let changed_operation_path = temp.path().join("changed-brokered-operation.json");
    fs::write(
        &changed_operation_path,
        serde_json::to_vec_pretty(&changed_operation).unwrap(),
    )
    .unwrap();
    let rejected = run_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "operation",
            "execute",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&changed_operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "reject-changed-brokered-operation",
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!rejected
        .stderr
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
    let executed = run_cli(
        &daemon,
        &attachment_token,
        &[
            "operation",
            "execute",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "execute-brokered-operation",
        ],
    );

    let request = received
        .recv_timeout(Duration::from_secs(2))
        .expect("broker should send one HTTP request");
    assert!(request.contains("POST /effects/brokered HTTP/1.1"));
    assert!(request.contains(&format!("Authorization: Bearer {secret}")));
    assert!(!request.to_ascii_lowercase().contains("user-agent:"));
    assert!(!request.contains("tyrion-effect-"));
    assert!(request.ends_with("{\"release\":\"v1\"}"));
    assert_eq!(
        executed["operation_requests"][0]["status"], "confirmed",
        "{executed}"
    );
    assert_eq!(executed["credential_grants"][0]["status"], "consumed");
    let receipt = &executed["operation_requests"][0]["receipt"];
    assert_eq!(receipt["credential_delivery"], "brokered_stdin");
    assert_eq!(receipt["credential_revocation"], "verified_absent");
    assert_eq!(receipt["broker_process_contained"], true);
    assert_eq!(receipt["descendants_terminated"], true);
    assert_eq!(receipt["secret_material_retained"], false);
    assert_eq!(receipt["response_body_retained"], false);
    assert_eq!(receipt["secret_leak_detected"], false);
    let events = executed["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    for expected in [
        "credential_grant_issued",
        "operation_classified",
        "approval_gate_authorized",
        "operation_started",
        "operation_confirmed",
    ] {
        assert!(events.contains(&expected), "missing {expected} event");
    }
    assert!(!serde_json::to_vec(&executed)
        .unwrap()
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
    assert!(!keychain_contains(&keychain, credential_reference));
    assert_secret_absent_from_tree(temp.path(), secret.as_bytes());
}

#[test]
fn stranded_one_shot_exposure_is_contained_before_reconciliation() {
    let temp = TempDir::new().unwrap();
    let keychain = temp.path().join("credentials.keychain-db");
    create_keychain(&keychain);
    let credential_reference = "one-shot-release-token";
    let secret = "tyrion-one-shot-secret-14d0fd64";
    add_keychain_credential(&keychain, credential_reference, secret);
    let (destination, received) = start_http_effect_server();

    let openshell = write_executable(
        &temp.path().join("openshell"),
        include_str!("fixtures/fake_effect_openshell.sh"),
    );
    let adapter = write_executable(
        &temp.path().join("effect-adapter"),
        include_str!("fixtures/fake_effect_adapter.sh"),
    );
    let policy = temp.path().join("effect-policy.yaml");
    let destination_without_scheme = destination
        .strip_prefix("http://")
        .expect("test destination should use HTTP");
    let (host, port) = destination_without_scheme.split_once(':').unwrap();
    fs::write(
        &policy,
        format!(
            "version: 1\nfilesystem_policy:\n  include_workdir: false\n  read_only:\n    - /usr\n    - /lib\n    - /proc\n    - /sys/fs/cgroup\n    - /dev/urandom\n    - /etc\n  read_write:\n    - /sandbox\n    - /tmp\n    - /dev/null\nlandlock:\n  compatibility: hard_requirement\nprocess:\n  run_as_user: sandbox\n  run_as_group: sandbox\nnetwork_policies:\n  effect:\n    name: effect\n    endpoints:\n      - host: {host}\n        port: {port}\n        protocol: rest\n        access: full\n        enforcement: enforce\n    binaries:\n      - path: /sandbox/effect-adapter\n      - path: /usr/bin/curl\n"
        ),
    )
    .unwrap();
    let config_home = temp.path().join("openshell-config");
    fs::create_dir(&config_home).unwrap();
    let gateway = temp.path().join("gateway.toml");
    fs::write(
        &gateway,
        "[openshell.gateway]\ncompute_drivers = [\"vm\"]\n\n[openshell.gateway.mtls_auth]\nenabled = true\n\n[openshell.drivers.vm]\nvcpus = 2\nmem_mib = 2048\noverlay_disk_mib = 4096\n",
    )
    .unwrap();
    let kernel = temp.path().join("kernel.config");
    fs::write(
        &kernel,
        "CONFIG_SECURITY=y\nCONFIG_SECURITY_LANDLOCK=y\nCONFIG_LSM=\"landlock,lockdown,yama,integrity\"\nCONFIG_CGROUP_PIDS=y\nCONFIG_SECCOMP_FILTER=y\n",
    )
    .unwrap();
    let source_patch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/openshell/repaired-v0.0.104.patch");
    let runtime_path = temp.path().join("credential-runtime.json");
    fs::write(
        &runtime_path,
        serde_json::to_vec_pretty(&json!({
            "keychain": {
                "security_binary": "/usr/bin/security",
                "security_sha256": sha256_file(Path::new("/usr/bin/security")),
                "keychain_path": path_text(&keychain),
                "service": KEYCHAIN_SERVICE
            },
            "broker": {
                "curl_binary": "/usr/bin/curl",
                "curl_sha256": sha256_file(Path::new("/usr/bin/curl")),
                "destinations": {"dogfood-api": destination}
            },
            "effect_sandbox": {
                "openshell_binary": path_text(&openshell),
                "openshell_sha256": sha256_file(&openshell),
                "openshell_version": "openshell 0.0.104",
                "openshell_config_home": path_text(&config_home),
                "base_image": "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e",
                "policy_path": path_text(&policy),
                "policy_sha256": sha256_file(&policy),
                "gateway_config_path": path_text(&gateway),
                "gateway_config_sha256": sha256_file(&gateway),
                "kernel_config_path": path_text(&kernel),
                "kernel_config_sha256": sha256_file(&kernel),
                "runtime_artifacts": [{
                    "path": path_text(&openshell),
                    "sha256": sha256_file(&openshell)
                }],
                "source_revision": "dd2b4e3bc0688bdd59f90030f7c1d52511d6e354",
                "source_patch_path": path_text(&source_patch),
                "source_patch_sha256": sha256_file(&source_patch),
                "adapter_binary": path_text(&adapter),
                "adapter_sha256": sha256_file(&adapter),
                "adapter_version": "tyrion-effect-adapter 1.0.0",
                "destination": "dogfood-api",
                "vcpus": 2,
                "memory_mib": 2048,
                "overlay_disk_mib": 4096,
                "max_processes": 256
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &runtime_path,
        &["--fault-leave-one-shot-effect-started-before-cleanup"],
    );
    let attachment_token = connect_full_entry(&daemon, "one-shot-effect");
    let proposal_path = temp.path().join("one-shot-proposal.json");
    let mut one_shot_proposal = proposal();
    one_shot_proposal["authority"]["actions"] =
        json!(["deterministic.echo", "credential.command.request"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&one_shot_proposal).unwrap(),
    )
    .unwrap();
    let created = run_cli(
        &daemon,
        &attachment_token,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-one-shot-effect",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    run_cli(
        &daemon,
        &attachment_token,
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-one-shot-effect",
        ],
    );
    let running = wait_for(&daemon, &attachment_token, commission_id, |state| {
        state["attempts"].as_array().is_some_and(|attempts| {
            attempts
                .iter()
                .any(|attempt| attempt["status"] == "running")
        })
    });
    let attempt = running["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attempt| attempt["status"] == "running")
        .unwrap();
    let grant_path = temp.path().join("one-shot-grant.json");
    fs::write(
        &grant_path,
        serde_json::to_vec_pretty(&json!({
            "assignment_id": attempt["assignment_id"],
            "attempt_id": attempt["id"],
            "worker_lease_id": attempt["lease"]["id"],
            "mandate_revision": 1,
            "plan_revision": 1,
            "credential_reference": credential_reference,
            "capability": "http_bearer",
            "destination": "dogfood-api",
            "exposure": "one_shot",
            "credential_expires_at": unix_timestamp() + 300,
            "revocation": "delete_from_keychain"
        }))
        .unwrap(),
    )
    .unwrap();
    let granted = run_principal_cli(
        &daemon,
        &[
            "principal",
            "grant-credential",
            commission_id,
            "--file",
            path_text(&grant_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "grant-one-shot-credential",
        ],
    );
    let grant_id = granted["credential_grants"][0]["id"].as_str().unwrap();
    let operation = json!({
        "assignment_id": attempt["assignment_id"],
        "attempt_id": attempt["id"],
        "worker_lease_id": attempt["lease"]["id"],
        "mandate_revision": 1,
        "plan_revision": 1,
        "operation": "credential.command.request",
        "repository": null,
        "target": "effects/one-shot",
        "parameters": {
            "body": "{\"release\":\"v2\"}",
            "content_type": "application/json",
            "method": "POST",
            "reconciliation_target": "effects/one-shot",
            "confirmed_reconciliation_sha256": format!("{:x}", Sha256::digest(b"applied")),
            "not_applied_reconciliation_sha256": format!("{:x}", Sha256::digest(b"not-applied"))
        },
        "destination": "dogfood-api",
        "effect": "external.write",
        "credential": {"grant_id": grant_id, "mode": "one_shot_exposure"},
        "consequences": ["Create the exact one-shot dogfood release marker"],
        "limits": {
            "max_output_bytes": 1024,
            "max_duration_seconds": 5,
            "max_paid_service_spend_cents": 0
        }
    });
    let operation_path = temp.path().join("one-shot-operation.json");
    fs::write(
        &operation_path,
        serde_json::to_vec_pretty(&operation).unwrap(),
    )
    .unwrap();
    let proposed = run_cli(
        &daemon,
        &attachment_token,
        &[
            "operation",
            "propose",
            commission_id,
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "propose-one-shot-operation",
        ],
    );
    let gate = &proposed["approval_gates"][0];
    let authorized = run_principal_cli(
        &daemon,
        &[
            "principal",
            "approve-gate",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--expected-operation-digest",
            gate["operation_digest"].as_str().unwrap(),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "approve-one-shot-operation",
        ],
    );
    assert_eq!(
        authorized["credential_exposure_grants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        authorized["credential_exposure_grants"][0]["status"],
        "authorized"
    );
    let interrupted = run_cli_output(
        &daemon,
        Some(&attachment_token),
        &[
            "operation",
            "execute",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "execute-one-shot-operation",
        ],
    );
    assert!(!interrupted.status.success());

    let request = received
        .recv_timeout(Duration::from_secs(2))
        .expect("Effect Sandbox should send one HTTP request");
    assert!(request.contains(&format!("Authorization: Bearer {secret}")));
    assert!(request.contains("POST /effects/one-shot HTTP/1.1"));
    let sandbox_log_before_restart =
        fs::read_to_string(temp.path().join("fake-effect-openshell/commands.log")).unwrap();
    assert_eq!(
        sandbox_log_before_restart.matches("sandbox create").count(),
        1
    );
    assert_eq!(
        sandbox_log_before_restart.matches("sandbox delete").count(),
        0
    );
    assert!(keychain_contains(&keychain, credential_reference));

    daemon.restart();
    let recovered = run_cli(
        &daemon,
        &attachment_token,
        &["commission", "inspect", commission_id],
    );
    assert_eq!(recovered["commission"]["status"], "paused");
    assert_eq!(recovered["operation_requests"][0]["status"], "uncertain");
    let receipt = &recovered["operation_requests"][0]["receipt"];
    assert_eq!(receipt["containment_confirmed"], true);
    assert_eq!(receipt["cleanup"]["sandbox_destroyed"], true);
    assert_eq!(receipt["cleanup"]["descendants_terminated"], true);
    assert_eq!(receipt["cleanup"]["secret_leak_detected"], false);
    assert_eq!(
        receipt["cleanup"]["credential_revocation"],
        "verified_absent"
    );
    let sandbox_log =
        fs::read_to_string(temp.path().join("fake-effect-openshell/commands.log")).unwrap();
    assert_eq!(sandbox_log.matches("sandbox create").count(), 1);
    assert_eq!(sandbox_log.matches("sandbox delete").count(), 1);
    assert!(sandbox_log.contains("--no-auto-providers"));
    assert!(sandbox_log.contains("descendant-terminated"));
    assert!(!sandbox_log.contains(secret));
    assert!(!keychain_contains(&keychain, credential_reference));
    assert_secret_absent_from_tree(temp.path(), secret.as_bytes());
}

#[test]
fn lost_acknowledgement_reconciles_read_only_without_replaying_the_effect() {
    let temp = TempDir::new().unwrap();
    let keychain = temp.path().join("credentials.keychain-db");
    create_keychain(&keychain);
    let credential_reference = "reconciled-release-token";
    let secret = "tyrion-reconcile-secret-7fdf4c30";
    add_keychain_credential(&keychain, credential_reference, secret);
    let (destination, requests, release_post) = start_reconcilable_effect_server(secret.to_owned());
    let runtime_path = temp.path().join("credential-runtime.json");
    fs::write(
        &runtime_path,
        serde_json::to_vec_pretty(&json!({
            "keychain": {
                "security_binary": "/usr/bin/security",
                "security_sha256": sha256_file(Path::new("/usr/bin/security")),
                "keychain_path": path_text(&keychain),
                "service": KEYCHAIN_SERVICE
            },
            "broker": {
                "curl_binary": "/usr/bin/curl",
                "curl_sha256": sha256_file(Path::new("/usr/bin/curl")),
                "destinations": {"dogfood-api": destination}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut daemon = RunningDaemon::start(temp.path(), &runtime_path);
    let attachment_token = connect_full_entry(&daemon, "reconciled-effect");
    let proposal_path = temp.path().join("reconciled-proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal()).unwrap(),
    )
    .unwrap();
    let created = run_cli(
        &daemon,
        &attachment_token,
        &[
            "proposal",
            "create",
            "--file",
            path_text(&proposal_path),
            "--idempotency-key",
            "create-reconciled-effect",
        ],
    );
    let commission_id = created["commission"]["id"].as_str().unwrap();
    run_cli(
        &daemon,
        &attachment_token,
        &[
            "commission",
            "accept",
            commission_id,
            "--expected-revision",
            "0",
            "--idempotency-key",
            "accept-reconciled-effect",
        ],
    );
    let running = wait_for(&daemon, &attachment_token, commission_id, |state| {
        state["attempts"].as_array().is_some_and(|attempts| {
            attempts
                .iter()
                .any(|attempt| attempt["status"] == "running")
        })
    });
    let attempt = running["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attempt| attempt["status"] == "running")
        .unwrap();
    let grant_path = temp.path().join("reconciled-grant.json");
    fs::write(
        &grant_path,
        serde_json::to_vec_pretty(&json!({
            "assignment_id": attempt["assignment_id"],
            "attempt_id": attempt["id"],
            "worker_lease_id": attempt["lease"]["id"],
            "mandate_revision": 1,
            "plan_revision": 1,
            "credential_reference": credential_reference,
            "capability": "http_bearer",
            "destination": "dogfood-api",
            "exposure": "brokered_only",
            "credential_expires_at": unix_timestamp() + 300,
            "revocation": "delete_from_keychain"
        }))
        .unwrap(),
    )
    .unwrap();
    let granted = run_principal_cli(
        &daemon,
        &[
            "principal",
            "grant-credential",
            commission_id,
            "--file",
            path_text(&grant_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "grant-reconciled-credential",
        ],
    );
    let grant_id = granted["credential_grants"][0]["id"].as_str().unwrap();
    let confirmed_sha256 = format!("{:x}", Sha256::digest(b"applied"));
    let not_applied_sha256 = format!("{:x}", Sha256::digest(b"not-applied"));
    let operation = json!({
        "assignment_id": attempt["assignment_id"],
        "attempt_id": attempt["id"],
        "worker_lease_id": attempt["lease"]["id"],
        "mandate_revision": 1,
        "plan_revision": 1,
        "operation": "credential.http.request",
        "repository": null,
        "target": "effects/reconciled",
        "parameters": {
            "body": "{\"release\":\"v3\"}",
            "content_type": "application/json",
            "method": "POST",
            "reconciliation_target": "effects/reconciled",
            "confirmed_reconciliation_sha256": confirmed_sha256,
            "not_applied_reconciliation_sha256": not_applied_sha256
        },
        "destination": "dogfood-api",
        "effect": "external.write",
        "credential": {"grant_id": grant_id, "mode": "brokered"},
        "consequences": ["Create one release marker without duplicate replay"],
        "limits": {
            "max_output_bytes": 1024,
            "max_duration_seconds": 5,
            "max_paid_service_spend_cents": 0
        }
    });
    let operation_path = temp.path().join("reconciled-operation.json");
    fs::write(
        &operation_path,
        serde_json::to_vec_pretty(&operation).unwrap(),
    )
    .unwrap();
    let proposed = run_cli(
        &daemon,
        &attachment_token,
        &[
            "operation",
            "propose",
            commission_id,
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "propose-reconciled-operation",
        ],
    );
    let gate = &proposed["approval_gates"][0];
    run_principal_cli(
        &daemon,
        &[
            "principal",
            "approve-gate",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--expected-operation-digest",
            gate["operation_digest"].as_str().unwrap(),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "approve-reconciled-operation",
        ],
    );
    let mut interrupted = Command::new(env!("CARGO_BIN_EXE_tyrion"));
    interrupted
        .args(["--socket", path_text(&daemon.socket_path)])
        .args(["--attachment-token", &attachment_token])
        .args([
            "operation",
            "execute",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "execute-reconciled-operation",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let interrupted = interrupted.spawn().expect("effect CLI should start");
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(2)).unwrap(),
        "POST /effects/reconciled"
    );

    daemon.restart();
    release_post.send(()).unwrap();
    let interrupted = interrupted.wait_with_output().unwrap();
    assert!(!interrupted.status.success());
    let recovered = run_cli(
        &daemon,
        &attachment_token,
        &["commission", "inspect", commission_id],
    );
    assert_eq!(recovered["commission"]["status"], "paused");
    assert_eq!(recovered["operation_requests"][0]["status"], "uncertain");
    assert_eq!(
        recovered["operation_requests"][0]["receipt"]["recovered_after_control_plane_restart"],
        true
    );
    assert_eq!(
        recovered["operation_requests"][0]["receipt"]["cleanup"]["broker_process_contained"], true,
        "{recovered}"
    );
    assert_eq!(
        recovered["operation_requests"][0]["receipt"]["cleanup"]["credential_revocation"],
        "verified_absent"
    );
    assert!(!keychain_contains(&keychain, credential_reference));
    let replayed = run_cli(
        &daemon,
        &attachment_token,
        &[
            "operation",
            "execute",
            commission_id,
            gate["id"].as_str().unwrap(),
            "--file",
            path_text(&operation_path),
            "--expected-revision",
            "1",
            "--idempotency-key",
            "execute-reconciled-operation",
        ],
    );
    assert_eq!(replayed["operation_requests"][0]["status"], "uncertain");
    assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    let reconciled = run_principal_cli(
        &daemon,
        &[
            "principal",
            "reconcile-operation",
            commission_id,
            recovered["operation_requests"][0]["id"].as_str().unwrap(),
            "--outcome",
            "confirmed",
            "--observed-sha256",
            &confirmed_sha256,
            "--expected-revision",
            "1",
            "--idempotency-key",
            "reconcile-credentialed-effect",
        ],
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(2)).unwrap(),
        "GET /effects/reconciled"
    );
    assert_eq!(reconciled["operation_requests"][0]["status"], "confirmed");
    assert_eq!(
        reconciled["operation_requests"][0]["receipt"]["reconciliation"],
        "confirmed_after_restart"
    );
}

fn proposal() -> Value {
    json!({
        "goal": "hold one Worker while a credentialed effect executes",
        "criteria": [{
            "id": "held",
            "description": "The held Worker eventually returns the accepted Goal",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {
                "kind": "exact_match",
                "expected": "hold one Worker while a credentialed effect executes"
            }
        }],
        "authority": {
            "repositories": [],
            "paths": [],
            "actions": ["deterministic.echo", "credential.http.request"],
            "destinations": ["dogfood-api"],
            "effects": ["external.write"]
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

fn start_http_effect_server() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("effect request should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream
                .read(&mut buffer)
                .expect("request should be readable");
            assert!(read > 0, "request ended before its headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream
                .read(&mut buffer)
                .expect("request body should be readable");
            assert!(read > 0, "request ended before its body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        sender
            .send(String::from_utf8(bytes).expect("test request should be UTF-8"))
            .unwrap();
        stream
            .write_all(
                b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"created\":true}",
            )
            .unwrap();
    });
    (format!("http://{address}"), receiver)
}

fn start_reconcilable_effect_server(
    secret: String,
) -> (String, mpsc::Receiver<String>, mpsc::SyncSender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::sync_channel(2);
    let (release_sender, release_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut applied = false;
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("effect request should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream
                    .read(&mut buffer)
                    .expect("request should be readable");
                assert!(read > 0, "request ended before its headers");
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let first_line = headers.lines().next().unwrap().to_owned();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + content_length {
                let read = stream
                    .read(&mut buffer)
                    .expect("request body should be readable");
                assert!(read > 0, "request ended before its body");
                bytes.extend_from_slice(&buffer[..read]);
            }
            if first_line.starts_with("POST ") {
                assert!(headers.contains(&format!("Authorization: Bearer {secret}")));
                applied = true;
                sender.send("POST /effects/reconciled".into()).unwrap();
                release_receiver
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test should release the stranded POST");
                let _ = stream.write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncreated",
                );
            } else {
                assert_eq!(first_line, "GET /effects/reconciled HTTP/1.1");
                assert!(!headers.contains("Authorization:"));
                sender.send("GET /effects/reconciled".into()).unwrap();
                let body = if applied { "applied" } else { "not-applied" };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
            }
        }
    });
    (format!("http://{address}"), receiver, release_sender)
}

fn create_keychain(path: &Path) {
    successful_command(Command::new("/usr/bin/security").args([
        "create-keychain",
        "-p",
        KEYCHAIN_PASSWORD,
        path_text(path),
    ]));
    successful_command(Command::new("/usr/bin/security").args([
        "unlock-keychain",
        "-p",
        KEYCHAIN_PASSWORD,
        path_text(path),
    ]));
}

fn add_keychain_credential(keychain: &Path, account: &str, secret: &str) {
    let encoded = secret
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let output = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            "-X",
            &encoded,
            "-A",
            path_text(keychain),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "credential provisioning failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn keychain_contains(keychain: &Path, account: &str) -> bool {
    Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            path_text(keychain),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn successful_command(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_secret_absent_from_tree(root: &Path, secret: &[u8]) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_secret_absent_from_tree(&path, secret);
        } else if path.is_file() {
            let bytes = fs::read(&path).unwrap();
            assert!(
                !bytes.windows(secret.len()).any(|window| window == secret),
                "secret leaked to {}",
                path.display()
            );
        }
    }
}

fn connect_full_entry(daemon: &RunningDaemon, label: &str) -> String {
    let issued = run_cli_without_attachment(
        daemon,
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
    let connected = run_cli_without_attachment(
        daemon,
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
            &format!("connect-{label}"),
        ],
    );
    connected["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn wait_for(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let state = run_cli(
            daemon,
            attachment_token,
            &["commission", "inspect", commission_id],
        );
        if predicate(&state) {
            return state;
        }
        last = state;
        thread::sleep(Duration::from_millis(20));
    }
    panic!("Commission did not reach the expected state: {last}");
}

fn run_cli(daemon: &RunningDaemon, attachment_token: &str, arguments: &[&str]) -> Value {
    successful_json(
        Command::new(env!("CARGO_BIN_EXE_tyrion"))
            .args(["--socket", path_text(&daemon.socket_path)])
            .args(["--attachment-token", attachment_token])
            .args(arguments)
            .output()
            .expect("CLI should run"),
    )
}

fn run_cli_without_attachment(daemon: &RunningDaemon, arguments: &[&str]) -> Value {
    successful_json(
        Command::new(env!("CARGO_BIN_EXE_tyrion"))
            .args(["--socket", path_text(&daemon.socket_path)])
            .args(arguments)
            .output()
            .expect("CLI should run"),
    )
}

fn run_principal_cli(daemon: &RunningDaemon, arguments: &[&str]) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .arg("--principal-token-stdin")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Principal CLI should run");
    writeln!(child.stdin.as_mut().unwrap(), "{}", daemon.principal_token).unwrap();
    successful_json(child.wait_with_output().unwrap())
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
    command.args(arguments).output().expect("CLI should run")
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

fn sha256_file(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn write_executable(path: &Path, content: &str) -> PathBuf {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    path.to_owned()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

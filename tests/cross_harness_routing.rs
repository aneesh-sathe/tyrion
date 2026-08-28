#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct RunningDaemon {
    child: Child,
    socket_path: PathBuf,
}

struct RunningPi {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    request_sequence: u64,
}

impl RunningPi {
    fn start(socket_path: &Path) -> Self {
        let fake_pi = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pi_host.mjs");
        let mut child = Command::new(env!("CARGO_BIN_EXE_tyrion"))
            .args(["--socket", path_text(socket_path), "pi", "--pi-command"])
            .arg(fake_pi)
            .args(["--", "--mode", "rpc", "--no-session"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Pi Entry Session should launch");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            request_sequence: 0,
        }
    }

    fn request(&mut self, request_type: &str, message: Option<&str>) -> Value {
        self.request_sequence += 1;
        let id = format!("pi-control-{}", self.request_sequence);
        let request = json!({
            "id": id,
            "type": request_type,
            "message": message,
        });
        serde_json::to_writer(&mut self.input, &request).unwrap();
        self.input.write_all(b"\n").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(self.output.read_line(&mut line).unwrap(), 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["type"] == "response" && response["id"] == id {
                assert_eq!(response["success"], true, "Pi request failed: {response}");
                return response;
            }
        }
    }

    fn prompt(&mut self, message: &str) {
        self.request("prompt", Some(message));
    }

    fn messages(&mut self) -> Vec<Value> {
        self.request("get_messages", None)["data"]["messages"]
            .as_array()
            .unwrap()
            .clone()
    }
}

impl Drop for RunningPi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn skill_version(name: &str) -> Value {
    let marker = match name {
        "code-review" => "1",
        "backend" => "2",
        "frontend" => "3",
        _ => "f",
    };
    json!({
        "name": name,
        "content_digest": format!("sha256:{}", marker.repeat(64)),
    })
}

impl RunningDaemon {
    fn start(data_dir: &Path, worker_catalog: &Path) -> Self {
        Self::start_with_arguments(data_dir, worker_catalog, &["--fault-defer-ready-dispatch"])
    }

    fn start_with_arguments(
        data_dir: &Path,
        worker_catalog: &Path,
        extra_arguments: &[&str],
    ) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let root = data_dir.parent().unwrap();
        fs::create_dir_all(root.join("fake-openshell")).unwrap();
        let fake_openshell = write_executable(
            &root.join("openshell"),
            include_str!("fixtures/fake_openshell.sh"),
        );
        let fake_codex =
            write_executable(&root.join("codex"), include_str!("fixtures/fake_codex.sh"));
        let fake_claude = write_executable(
            &root.join("claude"),
            include_str!("fixtures/fake_claude.sh"),
        );
        let worker_config = write_runtime_fixture(root, &fake_openshell, &fake_codex, &fake_claude);
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyriond"));
        command
            .args([
                "--data-dir",
                path_text(data_dir),
                "--socket",
                path_text(&socket_path),
                "--worker-catalog",
                path_text(worker_catalog),
                "--codex-worker-config",
                path_text(&worker_config),
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
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn routed_native_skill_versions_are_pinned_and_recorded_with_verified_results() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut catalog = worker_catalog();
    let versions = [
        ("code-review", format!("sha256:{}", "1".repeat(64))),
        ("backend", format!("sha256:{}", "2".repeat(64))),
        ("frontend", format!("sha256:{}", "3".repeat(64))),
    ];
    catalog["configurations"][1]["skills"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "name": "backend",
            "content_digest": versions[1].1
        }));
    catalog["configurations"][1]["selected_skills"] = json!([{
        "name": "frontend",
        "content_digest": versions[2].1
    }]);
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "skill-version-entry");
    let proposal_path = temp.path().join("skill-version-proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&json!({
            "goal": "return a routed greeting",
            "execution": {"kind": "deterministic"},
            "worker_requirements": {
                "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                "tools": ["git"],
                "skills": [{
                    "name": "code-review",
                    "content_digest": versions[0].1
                }],
                "selected_skills": [{
                    "version": {
                        "name": "backend",
                        "content_digest": versions[1].1
                    },
                    "provenance": "principal"
                }],
                "min_context_tokens": 100000,
                "context_strategy": "fresh_with_retrieval",
                "assignment_constraints": ["coding"]
            },
            "criteria": [{
                "id": "greeting",
                "description": "The Result contains the routed greeting",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier_configuration": "deterministic-exact-match-v1",
                "verification_environment": "tyrion-controlled-v1",
                "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
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
                "max_model_spend_cents": 100,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "versioned-native-skill",
    );
    let defaults = accepted["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert_eq!(defaults.len(), 1);
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "code-review"
            && skill["content_digest"] == versions[0].1
            && skill["requirement"] == "required"
            && skill["provenance"] == "principal"
            && skill["delegation"] == "native_unchanged"
    }));
    assert_eq!(
        accepted["commission"]["authority"]["actions"],
        json!(["deterministic.echo"])
    );
    assert_eq!(accepted["credential_grants"], json!([]));

    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "verified_complete",
    );
    let defaults = completed["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert_eq!(defaults.len(), 3);
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "backend"
            && skill["content_digest"] == versions[1].1
            && skill["requirement"] == "selected"
            && skill["provenance"] == "principal"
    }));
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "frontend"
            && skill["content_digest"] == versions[2].1
            && skill["requirement"] == "selected"
            && skill["provenance"] == "worker"
    }));
    let result_skills = completed["results"][0]["skill_executions"]
        .as_array()
        .unwrap();
    assert_eq!(result_skills.len(), 3);
    assert!(result_skills.iter().all(|skill| {
        skill["worker_configuration"] == "claude-opus-review"
            && skill["assignment_class"] == "critical_path"
            && skill["verification_outcome"] == "passed"
            && skill["corrections"] == 0
            && skill["cost_cents"] == 0
            && skill["latency_ms"].as_u64().is_some()
            && skill["principal_intervention"] == false
    }));
    assert_eq!(completed["skill_associations"].as_array().unwrap().len(), 3);
    assert!(completed["skill_associations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|association| {
            association["causal"] == false
                && association["global_ban"] == false
                && association["confidence_basis_points"].as_u64().is_some()
                && association["observed_at"].as_i64().is_some()
                && !association["evidence"].as_array().unwrap().is_empty()
        }));
}

#[test]
fn worker_selected_native_skill_is_pinned_only_after_observed_invocation() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "worker-skill-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("invoke native Worker-selected Skill");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "worker-selected-native-skill",
    );
    let accepted_defaults = accepted["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert_eq!(accepted_defaults.len(), 1);
    assert!(accepted_defaults
        .iter()
        .all(|skill| skill["requirement"] == "required"));

    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "verified_complete",
    );
    let defaults = completed["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert_eq!(defaults.len(), 2);
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "frontend"
            && skill["content_digest"] == skill_version("frontend")["content_digest"]
            && skill["requirement"] == "selected"
            && skill["provenance"] == "worker"
            && skill["delegation"] == "native_unchanged"
    }));
    assert!(completed["results"][0]["skill_executions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|skill| skill["name"] == "frontend"));
}

#[test]
fn observed_worker_skill_remains_pinned_after_attempt_failure() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "failed-worker-skill-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("invoke native Worker-selected Skill then report structured failure");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "failed-worker-selected-native-skill",
    );
    let failed = wait_for_worker_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "failed",
    );
    let defaults = failed["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "frontend"
            && skill["content_digest"] == skill_version("frontend")["content_digest"]
            && skill["requirement"] == "selected"
            && skill["provenance"] == "worker"
    }));
}

#[test]
fn observed_worker_skill_remains_pinned_after_adapter_exit() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "exited-worker-skill-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("invoke native Worker-selected Skill then exit nonzero");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "exited-worker-selected-native-skill",
    );
    let failed = wait_for_worker_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "failed",
    );
    let defaults = failed["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert!(defaults.iter().any(|skill| {
        skill["name"] == "frontend"
            && skill["content_digest"] == skill_version("frontend")["content_digest"]
            && skill["requirement"] == "selected"
            && skill["provenance"] == "worker"
    }));
}

#[test]
fn harness_reported_required_skill_failure_reroutes_without_substitution_or_global_ban() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let digest = format!("sha256:{}", "1".repeat(64));
    let mut catalog = worker_catalog();
    catalog["configurations"][0]["skills"]
        .as_array_mut()
        .unwrap()
        .push(skill_version("frontend"));
    catalog["configurations"][1]["selected_skills"] = json!([skill_version("frontend")]);
    catalog["configurations"][0]["metrics"]["first_pass_acceptance"] = json!(9100);
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "skill-reroute-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("fail required Skill on preferred harness");
    proposal["worker_requirements"]["min_context_tokens"] = json!(0);
    proposal["worker_requirements"]
        .as_object_mut()
        .unwrap()
        .remove("context_strategy");
    proposal["worker_requirements"]["skills"] = json!([{
        "name": "code-review",
        "content_digest": digest
    }]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "required-skill-reroute",
    );
    assert_eq!(
        accepted["assignments"][0]["route"]["selected_configuration"]["id"],
        "claude-opus-review"
    );
    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "verified_complete",
    );
    let attempts = completed["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["worker_configuration"], "claude-opus-review");
    assert_eq!(attempts[0]["status"], "failed");
    assert_eq!(attempts[1]["worker_configuration"], "codex-deep");
    assert_eq!(attempts[1]["status"], "succeeded");
    let defaults = completed["assignments"][0]["skill_defaults"]
        .as_array()
        .unwrap();
    assert!(defaults.iter().all(|skill| skill["name"] != "frontend"));
    let associations = completed["skill_associations"].as_array().unwrap();
    let failure = associations
        .iter()
        .find(|association| association["observation"] == "required_skill_failure")
        .unwrap();
    assert_eq!(failure["skill_version"]["content_digest"], digest);
    assert_eq!(failure["worker_configuration"], "claude-opus-review");
    assert_eq!(failure["causal"], false);
    assert_eq!(failure["global_ban"], false);
    assert!(!failure["evidence"].as_array().unwrap().is_empty());
    let success = associations
        .iter()
        .find(|association| {
            association["observation"] == "verified_success"
                && association["skill_version"]["name"] == "code-review"
        })
        .unwrap();
    assert_eq!(success["skill_version"]["content_digest"], digest);
    assert_eq!(success["worker_configuration"], "codex-deep");
    assert!(completed["attention_conditions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|condition| condition["status"] == "resolved"));

    proposal["goal"] = json!("report malformed required Skill failure");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let malformed = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "malformed-required-skill-report",
    );
    let failed = wait_for_worker_status(
        &daemon,
        &attachment_token,
        malformed["commission"]["id"].as_str().unwrap(),
        "failed",
    );
    assert_eq!(failed["attempts"].as_array().unwrap().len(), 1);
    assert!(failed["skill_associations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|association| association["observation"] != "required_skill_failure"));
}

#[test]
fn principal_and_plan_skill_selections_are_pinned_to_the_accepted_plan_revision() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let versions = [
        ("code-review", format!("sha256:{}", "1".repeat(64))),
        ("backend", format!("sha256:{}", "2".repeat(64))),
        ("frontend", format!("sha256:{}", "3".repeat(64))),
    ];
    let mut catalog = worker_catalog();
    for configuration in catalog["configurations"].as_array_mut().unwrap() {
        configuration["skills"] = json!(versions
            .iter()
            .map(|(name, content_digest)| json!({
                "name": name,
                "content_digest": content_digest
            }))
            .collect::<Vec<_>>());
    }
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "plan-skill-entry");
    let proposal_path = temp.path().join("plan-skill-proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&json!({
            "goal": "complete two skill-pinned assignments",
            "execution": {"kind": "deterministic"},
            "worker_requirements": {
                "capabilities": ["commission-only-marker"],
                "skills": [{
                    "name": "code-review",
                    "content_digest": versions[0].1
                }],
                "selected_skills": [{
                    "version": {
                        "name": "backend",
                        "content_digest": versions[1].1
                    },
                    "provenance": "principal"
                }]
            },
            "plan": {"assignments": [{
                "id": "backend-assignment",
                "goal": "complete backend work",
                "dependencies": [],
                "criterion_ids": ["backend"],
                "purpose": "critical_path",
                "read_scopes": [],
                "write_scopes": [],
                "resources": {
                    "concurrency_slots": 1,
                    "max_storage_bytes": 1048576,
                    "max_model_spend_cents": 50,
                    "max_paid_service_spend_cents": 0
                },
                "worker_requirements": {"selected_skills": [{
                    "version": {
                        "name": "frontend",
                        "content_digest": versions[2].1
                    },
                    "provenance": "plan"
                }]}
            }, {
                "id": "review-assignment",
                "goal": "complete review work",
                "dependencies": [],
                "criterion_ids": ["review"],
                "purpose": "independent_verification",
                "read_scopes": [],
                "write_scopes": [],
                "resources": {
                    "concurrency_slots": 1,
                    "max_storage_bytes": 1048576,
                    "max_model_spend_cents": 50,
                    "max_paid_service_spend_cents": 0
                },
                "worker_requirements": {}
            }]},
            "criteria": [{
                "id": "backend",
                "description": "backend work completes",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier_configuration": "deterministic-exact-match-v1",
                "verification_environment": "tyrion-controlled-v1",
                "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
            }, {
                "id": "review",
                "description": "review work completes",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier_configuration": "deterministic-exact-match-v1",
                "verification_environment": "tyrion-controlled-v1",
                "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
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
                "max_worker_concurrency": 2,
                "max_storage_bytes": 2097152,
                "max_model_spend_cents": 100,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "plan-skill-provenance",
    );
    assert!(accepted["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|assignment| assignment["skill_defaults"].as_array().unwrap())
        .all(|skill| skill["requirement"] == "required"));
    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        accepted["commission"]["id"].as_str().unwrap(),
        "verified_complete",
    );
    let backend = completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["logical_id"] == "backend-assignment")
        .unwrap();
    let review = completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assignment| assignment["logical_id"] == "review-assignment")
        .unwrap();
    assert!(backend["skill_defaults"]
        .as_array()
        .unwrap()
        .iter()
        .any(|skill| {
            skill["name"] == "backend"
                && skill["provenance"] == "principal"
                && skill["plan_revision"] == 1
        }));
    assert!(backend["skill_defaults"]
        .as_array()
        .unwrap()
        .iter()
        .any(|skill| {
            skill["name"] == "frontend"
                && skill["provenance"] == "plan"
                && skill["plan_revision"] == 1
        }));
    assert!(review["skill_defaults"]
        .as_array()
        .unwrap()
        .iter()
        .any(|skill| {
            skill["name"] == "backend"
                && skill["provenance"] == "principal"
                && skill["plan_revision"] == 1
        }));
    assert!(completed["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .all(|assignment| {
            assignment["skill_defaults"]
                .as_array()
                .unwrap()
                .iter()
                .any(|skill| {
                    skill["name"] == "code-review"
                        && skill["requirement"] == "required"
                        && skill["provenance"] == "principal"
                        && skill["content_digest"] == versions[0].1
                })
        }));
}

#[test]
fn codex_entry_routes_to_the_best_complete_claude_configuration() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start(&data_dir, &catalog);
    let attachment_token = connect_full_entry(&daemon, "codex", "codex-entry-session");
    let proposal_path = temp.path().join("proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&json!({
            "goal": "return a routed greeting",
            "execution": {"kind": "deterministic"},
            "worker_requirements": {
                "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                "tools": ["git"],
                "skills": [skill_version("code-review")],
                "min_context_tokens": 100000,
                "context_strategy": "fresh_with_retrieval",
                "assignment_constraints": ["coding"]
            },
            "criteria": [{
                "id": "greeting",
                "description": "The Result contains the routed greeting",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier_configuration": "deterministic-exact-match-v1",
                "verification_environment": "tyrion-controlled-v1",
                "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
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
                "max_model_spend_cents": 100,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();

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
            "create-routed-commission",
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
            "accept-routed-commission",
        ],
    );

    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["status"], "selected");
    assert_eq!(route["selected_configuration"]["id"], "claude-opus-review");
    assert_eq!(route["selected_configuration"]["harness"], "claude");
    assert_eq!(route["selected_configuration"]["model"], "claude-opus-5");
    assert_eq!(
        route["selected_configuration"]["settings"]["effort"],
        "high"
    );
    assert_eq!(route["selected_configuration"]["tools"], json!(["git"]));
    assert_eq!(
        route["selected_configuration"]["skills"],
        json!([skill_version("code-review"), skill_version("frontend")])
    );
    assert_eq!(
        route["selected_configuration"]["context"]["strategy"],
        "fresh_with_retrieval"
    );
    assert_eq!(route["rationale"]["entry_harness"], "codex");
    assert_eq!(
        route["rationale"]["entry_harness_preference_applied"],
        false
    );
    assert_eq!(
        route["rationale"]["ordering"],
        json!([
            "expected_verified_correctness",
            "preference_adherence",
            "first_pass_acceptance",
            "commission_elapsed_time_contribution",
            "cost",
            "continuity"
        ])
    );
    assert!(route["rationale"]["ineligible"]
        .as_array()
        .unwrap()
        .iter()
        .any(|candidate| candidate["configuration_id"] == "codex-fast"
            && candidate["failed_gates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gate| gate == "context_capacity")));
}

#[test]
fn qualified_pi_routes_and_executes_with_the_shared_worker_contract() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog, &[]);
    let attachment_token = connect_full_entry(&daemon, "pi", "pi-qualified-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["worker_requirements"]["require_configurations"] = json!(["pi-rpc-qualified"]);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "pi-qualified");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["status"], "selected");
    assert_eq!(route["selected_configuration"]["id"], "pi-rpc-qualified");
    assert_eq!(route["selected_configuration"]["harness"], "pi");
    assert_eq!(route["rationale"]["entry_harness"], "pi");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    assert_eq!(
        completed["workers"][0]["native_session_id"],
        "pi-session-fixture"
    );
    assert_eq!(completed["workers"][0]["usage"]["input_tokens"], 11);
    assert_eq!(
        completed["workers"][0]["latest_meaningful_activity"],
        "Result accepted"
    );
}

#[test]
fn incomplete_pi_worker_is_visibly_ineligible() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut catalog = worker_catalog();
    let pi = catalog["configurations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    pi["settings"]
        .as_object_mut()
        .unwrap()
        .remove("production_qualified");
    pi["adapter"]["command"] = json!([]);
    pi["adapter"]["sha256"] = json!("");
    pi["capabilities"] = json!([]);
    let catalog = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog, &[]);
    let attachment_token = connect_full_entry(&daemon, "pi", "pi-incomplete-entry");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["worker_requirements"]["require_configurations"] = json!(["pi-rpc-qualified"]);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "pi-incomplete");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["status"], "attention_required");
    let pi = route["rationale"]["ineligible"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["configuration_id"] == "pi-rpc-qualified")
        .unwrap();
    assert!(pi["failed_gates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gate| gate == "production_qualification"));
}

#[test]
fn claude_entry_does_not_bias_routing_toward_claude() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut catalog = worker_catalog();
    catalog["configurations"][0]["metrics"]["expected_verified_correctness"] = json!(9600);
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start(&data_dir, &catalog_path);
    let attachment_token = connect_full_entry(&daemon, "claude", "claude-entry-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "claude-entry");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["selected_configuration"]["id"], "codex-deep");
    assert_eq!(route["rationale"]["entry_harness"], "claude");
    assert_eq!(
        route["rationale"]["entry_harness_preference_applied"],
        false
    );
}

#[test]
fn every_declared_worker_eligibility_constraint_is_a_hard_gate() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut winner = worker_catalog()["configurations"][1].clone();
    winner["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("browser"));
    let mut candidates = vec![winner.clone()];
    let mut add_ineligible = |id: &str, mutate: fn(&mut Value)| {
        let mut candidate = winner.clone();
        candidate["id"] = json!(id);
        candidate["metrics"]["expected_verified_correctness"] = json!(9999);
        mutate(&mut candidate);
        candidates.push(candidate);
    };
    add_ineligible("missing-capability", |candidate| {
        candidate["capabilities"].as_array_mut().unwrap().pop();
    });
    add_ineligible("missing-tool", |candidate| candidate["tools"] = json!([]));
    add_ineligible("missing-skill", |candidate| {
        candidate["skills"] = json!([skill_version("frontend")])
    });
    add_ineligible("insufficient-context", |candidate| {
        candidate["context"]["capacity_tokens"] = json!(99999);
    });
    add_ineligible("wrong-context-strategy", |candidate| {
        candidate["context"]["strategy"] = json!("fresh");
    });
    add_ineligible("wrong-assignment", |candidate| {
        candidate["assignment_constraints"] = json!([]);
    });
    add_ineligible("insufficient-authority", |candidate| {
        candidate["authority_actions"] = json!([]);
    });
    add_ineligible("incompatible-authority-scope", |candidate| {
        candidate["authority_scope_types"] = json!(["action"]);
    });
    add_ineligible("insufficient-resources", |candidate| {
        candidate["resource_limits"]["max_storage_bytes"] = json!(1024);
    });
    let catalog_path =
        write_worker_catalog_value(temp.path(), json!({"configurations": candidates}));
    let daemon = RunningDaemon::start(&data_dir, &catalog_path);
    let attachment_token = connect_full_entry(&daemon, "codex", "hard-gate-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["worker_requirements"]["capabilities"] =
        json!(["structured_lifecycle", "semantic_interrupt", "browser"]);
    proposal["authority"]["paths"] = json!(["fixture"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "hard-gates");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["selected_configuration"]["id"], "claude-opus-review");
    let ineligible = route["rationale"]["ineligible"].as_array().unwrap();
    for (id, gate) in [
        ("missing-capability", "required_capabilities"),
        ("missing-tool", "required_tools"),
        ("missing-skill", "required_skills"),
        ("insufficient-context", "context_capacity"),
        ("wrong-context-strategy", "context_strategy"),
        ("wrong-assignment", "assignment_constraints"),
        ("insufficient-authority", "authority_compatibility"),
        (
            "incompatible-authority-scope",
            "authority_scope_compatibility",
        ),
        ("insufficient-resources", "resource_limits"),
    ] {
        let candidate = ineligible
            .iter()
            .find(|candidate| candidate["configuration_id"] == id)
            .unwrap_or_else(|| panic!("missing rationale for {id}"));
        assert_eq!(candidate["failed_gates"], json!([gate]));
    }
}

#[test]
fn selected_claude_adapter_drives_lifecycle_result_usage_and_session_identity() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "live-adapter-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "live-adapter");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();

    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    assert_eq!(
        completed["workers"][0]["configuration"]["harness"],
        "claude"
    );
    assert_eq!(
        completed["workers"][0]["native_session_id"],
        "claude-session-fixture"
    );
    assert_eq!(completed["workers"][0]["usage"]["input_tokens"], 10);
    assert_eq!(completed["workers"][0]["usage"]["output_tokens"], 5);
    assert_eq!(
        completed["workers"][0]["latest_meaningful_activity"],
        "Result accepted"
    );
    assert!(completed["events"].as_array().unwrap().iter().any(|event| {
        event["type"] == "worker_activity"
            && event["payload"]["activity"] == "Claude produced a structured Result"
    }));
    let log = fs::read_to_string(temp.path().join("fake-openshell/commands.log")).unwrap();
    let claude_policy = temp.path().join("hard-claude-policy.yaml");
    assert!(log.contains(&format!(
        "--policy {} --provider fixture-claude",
        path_text(&claude_policy)
    )));
    assert!(log.contains("/sandbox/claude --version"));
}

#[test]
fn structured_adapter_descendants_are_reaped_before_trace_collection_finishes() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "descendant-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("spawn structured descendant");
    proposal["criteria"][0]["verifier"]["expected"] = json!("return a routed greeting");
    proposal["worker_requirements"]["require_configurations"] = json!(["codex-deep"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "descendant");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    let log = fs::read_to_string(temp.path().join("fake-openshell/commands.log")).unwrap();
    assert!(log.contains("descendant-terminated"));
}

#[test]
fn one_commission_routes_independent_assignments_across_codex_and_claude() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start(&data_dir, &catalog);
    let attachment_token = connect_full_entry(&daemon, "codex", "codex-cross-session");
    let repository = temp.path().join("repository");
    let base_revision = create_repository(&repository);
    let proposal_path = temp.path().join("cross-harness-proposal.json");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&json!({
            "goal": "implement a split feature",
            "execution": {
                "kind": "codex_git",
                "repository": repository,
                "base_revision": base_revision
            },
            "criteria": [
                command_criterion("backend-check", "src/backend.txt"),
                command_criterion("frontend-check", "web/frontend.txt")
            ],
            "plan": {
                "assignments": [
                    {
                        "id": "backend",
                        "goal": "implement the backend",
                        "dependencies": [],
                        "criterion_ids": ["backend-check"],
                        "purpose": "critical_path",
                        "read_scopes": [],
                        "write_scopes": ["src/backend.txt"],
                        "resources": assignment_resources(),
                        "worker_requirements": {
                            "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                            "tools": ["git"],
                            "skills": [skill_version("backend")],
                            "min_context_tokens": 100000,
                            "assignment_constraints": ["coding"]
                        }
                    },
                    {
                        "id": "frontend",
                        "goal": "implement the frontend",
                        "dependencies": [],
                        "criterion_ids": ["frontend-check"],
                        "purpose": "critical_path",
                        "read_scopes": [],
                        "write_scopes": ["web/frontend.txt"],
                        "resources": assignment_resources(),
                        "worker_requirements": {
                            "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                            "tools": ["git"],
                            "skills": [skill_version("frontend")],
                            "min_context_tokens": 100000,
                            "assignment_constraints": ["coding"]
                        }
                    }
                ]
            },
            "authority": {
                "repositories": [repository],
                "paths": ["src/backend.txt", "web/frontend.txt"],
                "actions": ["codex.git_change"],
                "destinations": [],
                "effects": []
            },
            "resource_ceilings": {
                "max_attempts": 2,
                "max_elapsed_seconds": 30,
                "max_worker_concurrency": 2,
                "max_storage_bytes": 2097152,
                "max_model_spend_cents": 200,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "cross");
    let assignments = accepted["assignments"].as_array().unwrap();
    let backend = assignments
        .iter()
        .find(|assignment| assignment["logical_id"] == "backend")
        .unwrap();
    let frontend = assignments
        .iter()
        .find(|assignment| assignment["logical_id"] == "frontend")
        .unwrap();
    assert_eq!(
        backend["route"]["selected_configuration"]["id"],
        "codex-deep"
    );
    assert_eq!(
        frontend["route"]["selected_configuration"]["id"],
        "claude-opus-review"
    );
    assert_eq!(backend["route"]["rationale"]["entry_harness"], "codex");
    assert_eq!(frontend["route"]["rationale"]["entry_harness"], "codex");
}

#[test]
fn one_commission_executes_and_accepts_results_from_codex_and_claude_adapters() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog, &[]);
    let attachment_token = connect_full_entry(&daemon, "claude", "executing-cross-session");
    let proposal_path = temp.path().join("executing-cross-harness-proposal.json");
    let criterion = |id: &str| {
        json!({
            "id": id,
            "description": "The adapter returned the accepted Result",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier_configuration": "deterministic-exact-match-v1",
            "verification_environment": "tyrion-controlled-v1",
            "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
        })
    };
    let planned = |id: &str, criterion_id: &str, skill: &str| {
        json!({
            "id": id,
            "goal": "return a routed greeting",
            "dependencies": [],
            "criterion_ids": [criterion_id],
            "purpose": "critical_path",
            "read_scopes": [],
            "write_scopes": [],
            "resources": assignment_resources(),
            "worker_requirements": {
                "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                "tools": ["git"],
                "skills": [skill_version(skill)],
                "min_context_tokens": 100000,
                "assignment_constraints": ["coding"]
            }
        })
    };
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&json!({
            "goal": "collect one Result from each supported Worker adapter",
            "execution": {"kind": "deterministic"},
            "criteria": [criterion("codex-result"), criterion("claude-result")],
            "plan": {"assignments": [
                planned("codex-assignment", "codex-result", "backend"),
                planned("claude-assignment", "claude-result", "frontend")
            ]},
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
                "max_worker_concurrency": 2,
                "max_storage_bytes": 2097152,
                "max_model_spend_cents": 200,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();

    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "executing-cross",
    );
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    let workers = completed["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 2);
    let harnesses = workers
        .iter()
        .map(|worker| worker["configuration"]["harness"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        harnesses,
        std::collections::HashSet::from(["codex", "claude"])
    );
    assert!(workers.iter().all(|worker| {
        worker["native_session_id"]
            .as_str()
            .is_some_and(|session| !session.is_empty())
            && worker["usage"]["input_tokens"].as_u64().is_some()
    }));
    assert!(completed["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["status"] == "accepted"));
}

#[test]
fn unavailable_configuration_uses_only_an_approximately_equal_replacement() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut catalog = worker_catalog();
    catalog["configurations"][1]["available"] = json!(false);
    catalog["configurations"][0]["metrics"]["first_pass_acceptance"] = json!(9110);
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start(&data_dir, &catalog_path);
    let attachment_token = connect_full_entry(&daemon, "codex", "replacement-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "replacement");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["status"], "selected");
    assert_eq!(route["selected_configuration"]["id"], "codex-deep");
    assert_eq!(
        route["rationale"]["preferred_unavailable_configuration"],
        "claude-opus-review"
    );
    assert_eq!(route["rationale"]["automatic_replacement"], "codex-deep");
}

#[test]
fn unavailable_configuration_without_an_equal_replacement_requires_attention() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut catalog = worker_catalog();
    catalog["configurations"][1]["available"] = json!(false);
    let catalog_path = write_worker_catalog_value(temp.path(), catalog);
    let daemon = RunningDaemon::start(&data_dir, &catalog_path);
    let attachment_token = connect_full_entry(&daemon, "codex", "attention-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());

    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "attention");
    let route = &accepted["assignments"][0]["route"];
    assert_eq!(route["status"], "attention_required");
    assert_eq!(accepted["assignments"][0]["status"], "attention_required");
    assert!(route["selected_configuration"].is_null());
    assert!(accepted["execution_frontier"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        accepted["attention_conditions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        accepted["attention_conditions"][0]["code"],
        "worker_configuration_unavailable"
    );
    assert_eq!(
        accepted["attention_conditions"][0]["requirement"],
        route["rationale"]["attention_requirement"]
    );
}

#[test]
fn launch_unavailability_reroutes_only_to_an_approximately_equal_worker() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let unavailable_adapter = write_executable(
        &temp.path().join("unavailable-claude-adapter"),
        "#!/bin/sh\nexit 78\n",
    );
    let mut catalog: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["configurations"][1]["adapter"]["command"] = json!([unavailable_adapter]);
    catalog["configurations"][1]["adapter"]["sha256"] = json!(sha256_file(&unavailable_adapter));
    catalog["configurations"][0]["metrics"]["first_pass_acceptance"] = json!(9110);
    fs::write(&catalog_path, serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();

    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "launch-reroute-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "launch-reroute");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    assert_eq!(completed["workers"].as_array().unwrap().len(), 2);
    assert_eq!(completed["workers"][0]["status"], "failed");
    assert_eq!(
        completed["workers"][0]["configuration"]["id"],
        "claude-opus-review"
    );
    assert_eq!(completed["workers"][1]["configuration"]["id"], "codex-deep");
    assert_eq!(
        completed["assignments"][0]["route"]["rationale"]["preferred_unavailable_configuration"],
        "claude-opus-review"
    );
    assert!(completed["blockers"].as_array().unwrap().is_empty());
}

#[test]
fn launch_unavailability_without_equal_worker_opens_attention() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let unavailable_adapter = write_executable(
        &temp.path().join("unavailable-claude-adapter"),
        "#!/bin/sh\nexit 78\n",
    );
    let mut catalog: Value = serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    catalog["configurations"][1]["adapter"]["command"] = json!([unavailable_adapter]);
    catalog["configurations"][1]["adapter"]["sha256"] = json!(sha256_file(&unavailable_adapter));
    fs::write(&catalog_path, serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();

    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "launch-attention-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "launch-attention",
    );
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let attention = wait_for_assignment_status(
        &daemon,
        &attachment_token,
        commission_id,
        "attention_required",
    );
    assert_eq!(
        attention["attention_conditions"][0]["code"],
        "worker_configuration_unavailable"
    );
    assert!(attention["blockers"].as_array().unwrap().is_empty());
}

#[test]
fn attention_route_is_reconsidered_after_catalog_recovery_and_restart() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut unavailable = worker_catalog();
    unavailable["configurations"][1]["available"] = json!(false);
    let catalog_path = write_worker_catalog_value(temp.path(), unavailable);
    let daemon = RunningDaemon::start(&data_dir, &catalog_path);
    let attachment_token = connect_full_entry(&daemon, "codex", "recovery-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "recovery");
    let commission_id = accepted["commission"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        accepted["assignments"][0]["route"]["status"],
        "attention_required"
    );
    drop(daemon);

    write_worker_catalog_value(temp.path(), worker_catalog());
    let recovered = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let completed = wait_for_commission_status(
        &recovered,
        &attachment_token,
        &commission_id,
        "verified_complete",
    );
    assert_eq!(completed["assignments"][0]["route"]["status"], "selected");
    assert_eq!(completed["attention_conditions"][0]["status"], "resolved");
    assert_eq!(
        completed["workers"][0]["configuration"]["harness"],
        "claude"
    );
}

#[test]
fn selected_route_is_revalidated_against_the_restart_catalog() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let initial_catalog = write_worker_catalog(temp.path());
    let first = RunningDaemon::start(&data_dir, &initial_catalog);
    let attachment_token = connect_full_entry(&first, "codex", "selected-route-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let accepted = create_and_accept(
        &first,
        &attachment_token,
        &proposal_path,
        "selected-revalidation",
    );
    let commission_id = accepted["commission"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        accepted["assignments"][0]["route"]["selected_configuration"]["id"],
        "claude-opus-review"
    );
    drop(first);

    let mut replacement_catalog = worker_catalog();
    replacement_catalog["configurations"][0]["metrics"]["expected_verified_correctness"] =
        json!(9900);
    let replacement_catalog = write_worker_catalog_value(temp.path(), replacement_catalog);
    let second = RunningDaemon::start_with_arguments(&data_dir, &replacement_catalog, &[]);
    let completed = wait_for_commission_status(
        &second,
        &attachment_token,
        &commission_id,
        "verified_complete",
    );
    assert_eq!(
        completed["assignments"][0]["route"]["selected_configuration"]["id"],
        "codex-deep"
    );
    assert_eq!(completed["workers"][0]["configuration"]["id"], "codex-deep");
}

#[test]
fn active_entry_session_inspects_steers_and_interrupts_by_worker_handle() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(
        &data_dir,
        &catalog_path,
        &["--fault-hold-worker-for-control"],
    );
    let attachment_token = connect_full_entry(&daemon, "codex", "control-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["resource_ceilings"]["max_attempts"] = json!(2);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "control");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let running = wait_for_worker_status(&daemon, &attachment_token, commission_id, "running");
    let worker = &running["workers"][0];
    assert_eq!(worker["handle"], "Arya");
    assert!(worker["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(worker["assignment"]["logical_id"], "legacy-assignment");
    assert_eq!(worker["configuration"]["id"], "claude-opus-review");
    assert_eq!(worker["routing_rationale"]["entry_harness"], "codex");
    assert!(worker["elapsed_time_ms"].as_u64().is_some());
    assert_eq!(worker["latest_meaningful_activity"], "Worker launched");
    assert_eq!(
        worker["available_controls"],
        json!(["inspect", "steer", "interrupt"])
    );

    let steered = run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "worker",
            "steer",
            commission_id,
            "Arya",
            "--clarification",
            "Focus on the accepted greeting wording.",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "steer-arya",
        ],
    );
    assert_eq!(steered["commission"]["revision"], 1);
    assert_eq!(
        steered["workers"][0]["latest_meaningful_activity"],
        "Clarification delivered: Focus on the accepted greeting wording."
    );
    assert_eq!(steered["worker_commands"][0]["kind"], "steer");
    assert_eq!(steered["worker_commands"][0]["status"], "delivered");
    assert_eq!(steered["worker_commands"][0]["mandate_revision"], 1);
    assert_eq!(
        steered["commission"]["authority"],
        running["commission"]["authority"]
    );
    assert_eq!(steered["criteria"], running["criteria"]);

    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "worker",
            "interrupt",
            commission_id,
            "Arya",
            "--reason",
            "Principal requested a stop.",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "interrupt-arya",
        ],
    );
    let interrupted =
        wait_for_worker_status(&daemon, &attachment_token, commission_id, "interrupted");
    assert_eq!(interrupted["attempts"][0]["status"], "interrupted");
    assert_eq!(
        interrupted["attention_conditions"][0]["code"],
        "worker_interrupted"
    );
    assert_eq!(
        interrupted["workers"][0]["available_controls"],
        json!(["inspect", "retry"])
    );
    assert_eq!(interrupted["worker_commands"][1]["kind"], "interrupt");

    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "worker",
            "retry",
            commission_id,
            "Arya",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "retry-arya",
        ],
    );
    let retried = wait_for_worker_attempt(&daemon, &attachment_token, commission_id, 2, "running");
    assert_ne!(retried["workers"][1]["handle"], "Arya");
    assert_eq!(retried["attention_conditions"][0]["status"], "resolved");

    let second_handle = retried["workers"][1]["handle"].as_str().unwrap();
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "worker",
            "interrupt",
            commission_id,
            second_handle,
            "--reason",
            "Stop the final allowed attempt.",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "interrupt-final-attempt",
        ],
    );
    let exhausted =
        wait_for_worker_status(&daemon, &attachment_token, commission_id, "interrupted");
    assert_eq!(
        exhausted["workers"][1]["available_controls"],
        json!(["inspect"])
    );
}

#[test]
fn pi_native_commands_render_steer_interrupt_and_retry_controls() {
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(
        &data_dir,
        &catalog_path,
        &["--fault-hold-worker-for-control"],
    );
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(proposal_path).unwrap()).unwrap();
    proposal["resource_ceilings"]["max_attempts"] = json!(2);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);

    pi.prompt(&format!(
        "/tyrion-propose {}",
        serde_json::to_string(&proposal).unwrap()
    ));
    pi.prompt("/tyrion-accept");
    let running = wait_for_pi_worker(&mut pi, 1, "running");
    let running_content = running["content"].as_str().unwrap();
    assert!(running_content.contains("- Arya: running"));
    assert!(running_content.contains("controls: inspect, steer, interrupt"));
    assert!(running_content.contains("configuration claude-opus-review"));

    pi.prompt("/tyrion-steer Arya Focus on the accepted greeting wording.");
    let steered = latest_pi_projection(&mut pi);
    assert_eq!(steered["worker_commands"][0]["kind"], "steer");
    assert_eq!(steered["worker_commands"][0]["status"], "delivered");
    assert!(steered["workers"][0]["latest_meaningful_activity"]
        .as_str()
        .unwrap()
        .contains("accepted greeting wording"));

    pi.prompt("/tyrion-interrupt Arya Principal requested a stop.");
    let interrupted = wait_for_pi_worker(&mut pi, 1, "interrupted");
    assert!(interrupted["content"]
        .as_str()
        .unwrap()
        .contains("controls: inspect, retry"));
    assert_eq!(
        interrupted["details"]["commission"]["worker_commands"][1]["kind"],
        "interrupt"
    );

    pi.prompt("/tyrion-retry Arya");
    let retried = wait_for_pi_worker(&mut pi, 2, "running");
    let second_handle = retried["details"]["commission"]["workers"][1]["handle"]
        .as_str()
        .unwrap();
    assert_ne!(second_handle, "Arya");
    assert!(retried["content"].as_str().unwrap().contains(second_handle));
    assert_eq!(
        retried["details"]["commission"]["attention_conditions"][0]["status"],
        "resolved"
    );
}

#[test]
fn workers_without_live_adapter_controls_expose_only_inspection() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let mut configuration = worker_catalog()["configurations"][2].clone();
    configuration["id"] = json!("deterministic-local");
    configuration["harness"] = json!("tyrion");
    configuration["adapter"] = json!({"kind": "deterministic_local", "version": "1"});
    configuration["model"] = json!("deterministic-echo");
    configuration["settings"] = json!({});
    configuration["tools"] = json!([]);
    configuration["skills"] = json!([]);
    configuration["context"] = json!({
        "strategy": "exact_assignment",
        "capacity_tokens": u64::MAX,
    });
    configuration["capabilities"] = json!(["structured_lifecycle", "terminal_state"]);
    configuration["authority_actions"] = json!(["deterministic.echo"]);
    configuration["authority_scope_types"] = json!(["action"]);
    configuration["assignment_constraints"] = json!([]);
    configuration["containment_profile"] = json!("in-process-deterministic");
    configuration["replacement_class"] = json!("deterministic");
    let catalog_path =
        write_worker_catalog_value(temp.path(), json!({"configurations": [configuration]}));
    let daemon = RunningDaemon::start_with_arguments(
        &data_dir,
        &catalog_path,
        &["--fault-hold-worker-for-control"],
    );
    let attachment_token = connect_full_entry(&daemon, "codex", "inspect-only-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["worker_requirements"] = json!({});
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let accepted = create_and_accept(
        &daemon,
        &attachment_token,
        &proposal_path,
        "inspect-only-worker",
    );
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    let running = wait_for_worker_status(&daemon, &attachment_token, commission_id, "running");

    assert_eq!(
        running["workers"][0]["configuration"]["adapter"]["kind"],
        "deterministic_local"
    );
    assert_eq!(
        running["workers"][0]["available_controls"],
        json!(["inspect"])
    );
    let denied = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "worker",
            "steer",
            commission_id,
            "Arya",
            "--clarification",
            "This worker cannot receive steering.",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "deny-unsupported-steering",
        ])
        .output()
        .expect("CLI should run");
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("does not support steer"));
}

#[test]
fn failed_interrupt_delivery_does_not_interrupt_the_worker_locally() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&daemon, "codex", "broken-control-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("break structured control pipe");
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let accepted = create_and_accept(&daemon, &attachment_token, &proposal_path, "broken-pipe");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    wait_for_live_adapter_telemetry(
        &daemon,
        &attachment_token,
        commission_id,
        "claude-opus-review",
    );

    wait_for_path(&temp.path().join("fake-openshell/control-pipe-closed"));
    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(&daemon.socket_path)])
        .args([
            "--attachment-token",
            &attachment_token,
            "worker",
            "interrupt",
            commission_id,
            "Arya",
            "--reason",
            "Exercise the broken control pipe.",
            "--expected-revision",
            "1",
            "--idempotency-key",
            "broken-pipe-interrupt",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let completed = wait_for_commission_status(
        &daemon,
        &attachment_token,
        commission_id,
        "verified_complete",
    );
    assert_eq!(completed["workers"][0]["status"], "succeeded");
    assert_eq!(completed["worker_commands"][0]["status"], "failed");
}

#[test]
fn structured_adapters_receive_steering_and_interruption() {
    for configuration_id in ["codex-deep", "claude-opus-review", "pi-rpc-qualified"] {
        let temp = TempDir::new().expect("temporary directory should be created");
        let data_dir = temp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let catalog_path = write_worker_catalog(temp.path());
        let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
        let attachment_token =
            connect_full_entry(&daemon, "codex", &format!("control-{configuration_id}"));
        let proposal_path = write_deterministic_routing_proposal(temp.path());
        let mut proposal: Value =
            serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
        proposal["goal"] = json!("hold for structured control");
        proposal["criteria"][0]["verifier"]["expected"] = json!("hold for structured control");
        proposal["worker_requirements"]["require_configurations"] = json!([configuration_id]);
        if configuration_id == "pi-rpc-qualified" {
            proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
        }
        fs::write(
            &proposal_path,
            serde_json::to_vec_pretty(&proposal).unwrap(),
        )
        .unwrap();
        let accepted = create_and_accept(
            &daemon,
            &attachment_token,
            &proposal_path,
            &format!("native-control-{configuration_id}"),
        );
        let commission_id = accepted["commission"]["id"].as_str().unwrap();
        let running = wait_for_worker_status(&daemon, &attachment_token, commission_id, "running");
        assert_eq!(
            running["workers"][0]["configuration"]["id"],
            configuration_id
        );
        let live = wait_for_live_adapter_telemetry(
            &daemon,
            &attachment_token,
            commission_id,
            configuration_id,
        );
        assert!(live["workers"][0]["native_session_id"]
            .as_str()
            .is_some_and(|session| !session.is_empty()));
        assert!(live["workers"][0]["latest_meaningful_activity"]
            .as_str()
            .is_some_and(|activity| activity.ends_with("started")));

        run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "worker",
                "steer",
                commission_id,
                "Arya",
                "--clarification",
                "Preserve the accepted mandate.",
                "--expected-revision",
                "1",
                "--idempotency-key",
                &format!("steer-{configuration_id}"),
            ],
        );
        run_cli(
            &daemon.socket_path,
            &[
                "--attachment-token",
                &attachment_token,
                "worker",
                "interrupt",
                commission_id,
                "Arya",
                "--reason",
                "Stop the structured adapter.",
                "--expected-revision",
                "1",
                "--idempotency-key",
                &format!("interrupt-{configuration_id}"),
            ],
        );
        let interrupted =
            wait_for_worker_status(&daemon, &attachment_token, commission_id, "interrupted");
        assert_eq!(interrupted["attempts"][0]["status"], "interrupted");
        assert_eq!(interrupted["worker_commands"][0]["kind"], "steer");
        assert_eq!(interrupted["worker_commands"][1]["kind"], "interrupt");
        assert!(interrupted["workers"][0]["native_session_id"]
            .as_str()
            .is_some_and(|session| !session.is_empty()));
        let expected_usage = if configuration_id == "pi-rpc-qualified" {
            (19, 4)
        } else {
            (2, 0)
        };
        assert_eq!(
            interrupted["workers"][0]["usage"]["input_tokens"],
            expected_usage.0
        );
        assert_eq!(
            interrupted["workers"][0]["usage"]["output_tokens"],
            expected_usage.1
        );
    }
}

#[test]
fn failed_structured_terminals_retain_native_session_and_usage() {
    for configuration_id in ["codex-deep", "claude-opus-review", "pi-rpc-qualified"] {
        let temp = TempDir::new().expect("temporary directory should be created");
        let data_dir = temp.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        let catalog_path = write_worker_catalog(temp.path());
        let daemon = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
        let attachment_token =
            connect_full_entry(&daemon, "codex", &format!("failure-{configuration_id}"));
        let proposal_path = write_deterministic_routing_proposal(temp.path());
        let mut proposal: Value =
            serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
        proposal["goal"] = json!("report structured failure");
        proposal["worker_requirements"]["require_configurations"] = json!([configuration_id]);
        if configuration_id == "pi-rpc-qualified" {
            proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
        }
        fs::write(
            &proposal_path,
            serde_json::to_vec_pretty(&proposal).unwrap(),
        )
        .unwrap();
        let accepted = create_and_accept(
            &daemon,
            &attachment_token,
            &proposal_path,
            &format!("failure-{configuration_id}"),
        );
        let commission_id = accepted["commission"]["id"].as_str().unwrap();
        let failed = wait_for_worker_status(&daemon, &attachment_token, commission_id, "failed");
        assert!(failed["workers"][0]["native_session_id"]
            .as_str()
            .is_some_and(|session| !session.is_empty()));
        let expected_usage = if configuration_id == "pi-rpc-qualified" {
            (23, 7)
        } else {
            (6, 1)
        };
        assert_eq!(
            failed["workers"][0]["usage"]["input_tokens"],
            expected_usage.0
        );
        assert_eq!(
            failed["workers"][0]["usage"]["output_tokens"],
            expected_usage.1
        );
    }
}

#[test]
fn restart_recovers_a_stranded_structured_worker_and_retries() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    let catalog_path = write_worker_catalog(temp.path());
    let mut first = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let attachment_token = connect_full_entry(&first, "pi", "restart-control-session");
    let proposal_path = write_deterministic_routing_proposal(temp.path());
    let mut proposal: Value = serde_json::from_slice(&fs::read(&proposal_path).unwrap()).unwrap();
    proposal["goal"] = json!("hold for structured control");
    proposal["criteria"][0]["verifier"]["expected"] = json!("hold for structured control");
    proposal["resource_ceilings"]["max_attempts"] = json!(2);
    proposal["resource_ceilings"]["max_model_spend_cents"] = json!(0);
    proposal["worker_requirements"]["require_configurations"] = json!(["pi-rpc-qualified"]);
    fs::write(
        &proposal_path,
        serde_json::to_vec_pretty(&proposal).unwrap(),
    )
    .unwrap();
    let accepted = create_and_accept(&first, &attachment_token, &proposal_path, "crash-recovery");
    let commission_id = accepted["commission"]["id"].as_str().unwrap();
    wait_for_live_adapter_telemetry(&first, &attachment_token, commission_id, "pi-rpc-qualified");
    first.child.kill().unwrap();
    first.child.wait().unwrap();

    let second = RunningDaemon::start_with_arguments(
        &data_dir,
        &catalog_path,
        &["--fault-skip-sandbox-cleanup"],
    );
    let recovered_once = run_cli(
        &second.socket_path,
        &[
            "--attachment-token",
            &attachment_token,
            "commission",
            "inspect",
            commission_id,
        ],
    );
    assert_eq!(recovered_once["workers"].as_array().unwrap().len(), 1);
    assert_eq!(recovered_once["workers"][0]["status"], "failed");
    drop(second);

    let third = RunningDaemon::start_with_arguments(&data_dir, &catalog_path, &[]);
    let recovered = wait_for_worker_attempt(&third, &attachment_token, commission_id, 2, "running");
    assert!(recovered["workers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|worker| worker["status"] == "failed"));
    assert!(recovered["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|attempt| {
            attempt["status"] == "failed"
                && attempt["lease"]["status"] == "expired"
                && attempt["reservation"]["status"] == "revoked"
        }));
    let log = fs::read_to_string(temp.path().join("fake-openshell/commands.log")).unwrap();
    assert!(log.contains("sandbox delete"));
    assert!(log.contains("descendant-terminated"));
}

fn wait_for_live_adapter_telemetry(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    configuration_id: &str,
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
        if inspected["workers"][0]["configuration"]["id"] == configuration_id
            && inspected["workers"][0]["native_session_id"]
                .as_str()
                .is_some_and(|session| !session.is_empty())
            && inspected["workers"][0]["latest_meaningful_activity"]
                .as_str()
                .is_some_and(|activity| activity.ends_with("started"))
        {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Worker did not publish live adapter telemetry: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "Expected fixture signal was not created: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn latest_pi_message(pi: &mut RunningPi) -> Value {
    pi.messages()
        .into_iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .expect("Pi should render a Commission projection")
}

fn latest_pi_projection(pi: &mut RunningPi) -> Value {
    latest_pi_message(pi)["details"]["commission"].clone()
}

fn wait_for_pi_worker(pi: &mut RunningPi, expected_count: usize, expected_status: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pi.prompt("/tyrion-status");
        let message = latest_pi_message(pi);
        if message["details"]["commission"]["workers"]
            .as_array()
            .is_some_and(|workers| {
                workers.len() == expected_count
                    && workers
                        .last()
                        .is_some_and(|worker| worker["status"] == expected_status)
            })
        {
            return message;
        }
        assert!(
            Instant::now() < deadline,
            "Pi Worker did not reach {expected_status}: {message}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_worker_status(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    expected_status: &str,
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
        if inspected["workers"].as_array().is_some_and(|workers| {
            workers
                .first()
                .is_some_and(|worker| worker["status"] == expected_status)
        }) {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Worker did not reach {expected_status}: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_worker_attempt(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    expected_count: usize,
    expected_status: &str,
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
        if inspected["workers"].as_array().is_some_and(|workers| {
            workers.len() == expected_count
                && workers
                    .last()
                    .is_some_and(|worker| worker["status"] == expected_status)
        }) {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Worker retry did not reach {expected_status}: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_assignment_status(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    expected_status: &str,
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
        if inspected["assignments"][0]["status"] == expected_status {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Assignment did not reach {expected_status}: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_commission_status(
    daemon: &RunningDaemon,
    attachment_token: &str,
    commission_id: &str,
    expected_status: &str,
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
        if inspected["commission"]["status"] == expected_status {
            return inspected;
        }
        assert!(
            Instant::now() < deadline,
            "Commission did not reach {expected_status}: {inspected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn command_criterion(id: &str, path: &str) -> Value {
    json!({
        "id": id,
        "description": format!("{path} exists"),
        "required_evidence": "command_output",
        "verifier_type": "deterministic",
        "verification_depth": "standard",
        "verifier_configuration": "contained-command-v1",
        "verification_environment": "tyrion-controlled-v1",
        "verifier": {"kind": "command", "argv": ["test", "-f", path]}
    })
}

fn assignment_resources() -> Value {
    json!({
        "concurrency_slots": 1,
        "max_storage_bytes": 1048576,
        "max_model_spend_cents": 100,
        "max_paid_service_spend_cents": 0
    })
}

fn create_repository(path: &Path) -> String {
    fs::create_dir(path).unwrap();
    run_git(path, &["init", "-q"]);
    run_git(path, &["config", "user.name", "Tyrion Test"]);
    run_git(path, &["config", "user.email", "tyrion@example.invalid"]);
    fs::write(path.join("README.md"), "base\n").unwrap();
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-qm", "test: create base"]);
    run_git(path, &["rev-parse", "HEAD"])
}

fn run_git(path: &Path, arguments: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn write_deterministic_routing_proposal(root: &Path) -> PathBuf {
    let path = root.join("deterministic-routing-proposal.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "goal": "return a routed greeting",
            "execution": {"kind": "deterministic"},
            "worker_requirements": {
                "capabilities": ["structured_lifecycle", "semantic_interrupt"],
                "tools": ["git"],
                "skills": [skill_version("code-review")],
                "min_context_tokens": 100000,
                "context_strategy": "fresh_with_retrieval",
                "assignment_constraints": ["coding"]
            },
            "criteria": [{
                "id": "greeting",
                "description": "The Result contains the routed greeting",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier_configuration": "deterministic-exact-match-v1",
                "verification_environment": "tyrion-controlled-v1",
                "verifier": {"kind": "exact_match", "expected": "return a routed greeting"}
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
                "max_model_spend_cents": 100,
                "max_paid_service_spend_cents": 0
            },
            "known_uncertainties": []
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn create_and_accept(
    daemon: &RunningDaemon,
    attachment_token: &str,
    proposal_path: &Path,
    key: &str,
) -> Value {
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
            &format!("create-{key}"),
        ],
    );
    run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            attachment_token,
            "commission",
            "accept",
            created["commission"]["id"].as_str().unwrap(),
            "--expected-revision",
            "0",
            "--idempotency-key",
            &format!("accept-{key}"),
        ],
    )
}

fn write_worker_catalog(root: &Path) -> PathBuf {
    write_worker_catalog_value(root, worker_catalog())
}

fn write_worker_catalog_value(root: &Path, catalog: Value) -> PathBuf {
    let mut catalog = catalog;
    for configuration in catalog["configurations"].as_array_mut().unwrap() {
        let kind = configuration["adapter"]["kind"].as_str().unwrap();
        let qualified_pi =
            kind != "pi_rpc" || configuration["settings"]["production_qualified"] == true;
        if qualified_pi && matches!(kind, "codex_app_server" | "claude_agent_sdk" | "pi_rpc") {
            let adapter = Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/fake_structured_adapter.sh"
            ));
            configuration["adapter"]["command"] = json!([
                adapter,
                match kind {
                    "codex_app_server" => "codex",
                    "claude_agent_sdk" => "claude",
                    "pi_rpc" => "pi",
                    _ => unreachable!(),
                }
            ]);
            configuration["adapter"]["sha256"] = json!(sha256_file(adapter));
        }
    }
    let path = root.join("worker-catalog.json");
    fs::write(&path, serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();
    path
}

fn worker_catalog() -> Value {
    json!({
        "configurations": [
            {
                "id": "codex-deep",
                "harness": "codex",
                "adapter": {"kind": "codex_app_server", "version": "0.147.0"},
                "model": "gpt-5.3-codex",
                "settings": {"reasoning_effort": "xhigh"},
                "tools": ["git"],
                "skills": [skill_version("code-review"), skill_version("backend")],
                "context": {"strategy": "fresh_with_retrieval", "capacity_tokens": 400000},
                "resource_limits": {
                    "max_concurrency_slots": 1,
                    "max_storage_bytes": 2097152,
                    "max_model_spend_cents": 200,
                    "max_paid_service_spend_cents": 0
                },
                "capabilities": ["structured_lifecycle", "semantic_interrupt", "terminal_state", "usage", "skills", "result_submission", "contained"],
                "authority_actions": ["deterministic.echo", "codex.git_change"],
                "authority_scope_types": ["repository", "path", "action"],
                "assignment_constraints": ["coding"],
                "containment_profile": "openshell-repaired-v0.0.104",
                "replacement_class": "deep-coding",
                "available": true,
                "metrics": {
                    "expected_verified_correctness": 9400,
                    "preference_adherence": 9000,
                    "first_pass_acceptance": 9000,
                    "commission_elapsed_time_contribution_ms": 1200,
                    "cost_cents": 80,
                    "continuity": 5000
                }
            },
            {
                "id": "claude-opus-review",
                "harness": "claude",
                "adapter": {"kind": "claude_agent_sdk", "version": "0.2.0"},
                "model": "claude-opus-5",
                "settings": {"effort": "high"},
                "tools": ["git"],
                "skills": [skill_version("code-review"), skill_version("frontend")],
                "context": {"strategy": "fresh_with_retrieval", "capacity_tokens": 200000},
                "resource_limits": {
                    "max_concurrency_slots": 1,
                    "max_storage_bytes": 2097152,
                    "max_model_spend_cents": 200,
                    "max_paid_service_spend_cents": 0
                },
                "capabilities": ["structured_lifecycle", "semantic_interrupt", "terminal_state", "usage", "skills", "result_submission", "contained"],
                "authority_actions": ["deterministic.echo", "codex.git_change"],
                "authority_scope_types": ["repository", "path", "action"],
                "assignment_constraints": ["coding"],
                "containment_profile": "openshell-repaired-v0.0.104",
                "replacement_class": "deep-coding",
                "available": true,
                "metrics": {
                    "expected_verified_correctness": 9500,
                    "preference_adherence": 9100,
                    "first_pass_acceptance": 9200,
                    "commission_elapsed_time_contribution_ms": 1400,
                    "cost_cents": 90,
                    "continuity": 1000
                }
            },
            {
                "id": "codex-fast",
                "harness": "codex",
                "adapter": {"kind": "codex_app_server", "version": "0.147.0"},
                "model": "gpt-5.3-codex",
                "settings": {"reasoning_effort": "low"},
                "tools": ["git"],
                "skills": [skill_version("code-review")],
                "context": {"strategy": "fresh", "capacity_tokens": 64000},
                "resource_limits": {
                    "max_concurrency_slots": 1,
                    "max_storage_bytes": 2097152,
                    "max_model_spend_cents": 100,
                    "max_paid_service_spend_cents": 0
                },
                "capabilities": ["structured_lifecycle", "semantic_interrupt", "terminal_state", "usage", "skills", "result_submission", "contained"],
                "authority_actions": ["deterministic.echo"],
                "authority_scope_types": ["action"],
                "assignment_constraints": ["coding"],
                "containment_profile": "openshell-repaired-v0.0.104",
                "replacement_class": "fast-coding",
                "available": true,
                "metrics": {
                    "expected_verified_correctness": 8000,
                    "preference_adherence": 8500,
                    "first_pass_acceptance": 7800,
                    "commission_elapsed_time_contribution_ms": 500,
                    "cost_cents": 30,
                    "continuity": 9000
                }
            },
            {
                "id": "pi-rpc-qualified",
                "harness": "pi",
                "adapter": {"kind": "pi_rpc", "version": "1.0.0"},
                "model": "openai/fixture-pi",
                "settings": {"production_qualified": true},
                "tools": ["git"],
                "skills": [skill_version("code-review")],
                "context": {"strategy": "fresh_with_retrieval", "capacity_tokens": 200000},
                "resource_limits": {
                    "max_concurrency_slots": 1,
                    "max_storage_bytes": 2097152,
                    "max_model_spend_cents": 0,
                    "max_paid_service_spend_cents": 0
                },
                "capabilities": ["structured_lifecycle", "semantic_interrupt", "terminal_state", "usage", "skills", "result_submission", "contained"],
                "authority_actions": ["deterministic.echo", "codex.git_change"],
                "authority_scope_types": ["repository", "path", "action"],
                "assignment_constraints": ["coding"],
                "containment_profile": "openshell-repaired-v0.0.104",
                "replacement_class": "deep-coding",
                "available": true,
                "metrics": {
                    "expected_verified_correctness": 9300,
                    "preference_adherence": 9000,
                    "first_pass_acceptance": 8900,
                    "commission_elapsed_time_contribution_ms": 1300,
                    "cost_cents": 0,
                    "continuity": 4000
                }
            }
        ]
    })
}

fn connect_full_entry(daemon: &RunningDaemon, harness: &str, native_session_id: &str) -> String {
    let issued = run_cli(
        &daemon.socket_path,
        &[
            "attachment",
            "issue-token",
            "--harness",
            harness,
            "--adapter-identity",
            "routing-entry",
            "--adapter-version",
            "1.0.0",
            "--idempotency-key",
            "issue-routing-token",
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
            harness,
            "--adapter-identity",
            "routing-entry",
            "--adapter-version",
            "1.0.0",
            "--native-session-id",
            native_session_id,
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
            "connect-routing-session",
        ],
    );
    connected["attachment_session_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn write_runtime_fixture(root: &Path, openshell: &Path, codex: &Path, claude: &Path) -> PathBuf {
    let policy = root.join("hard-policy.yaml");
    fs::write(
        &policy,
        include_bytes!("../runtime/openshell/hard-landlock-policy.yaml"),
    )
    .unwrap();
    let claude_policy = root.join("hard-claude-policy.yaml");
    fs::write(
        &claude_policy,
        include_bytes!("../runtime/openshell/hard-landlock-claude-policy.yaml"),
    )
    .unwrap();
    let pi_policy = root.join("hard-pi-policy.yaml");
    fs::write(
        &pi_policy,
        include_bytes!("../runtime/openshell/hard-landlock-pi-policy.yaml"),
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
    fs::create_dir_all(&config_home).unwrap();
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
            "claude": {
                "policy_path": claude_policy,
                "policy_sha256": sha256_file(&claude_policy),
                "openshell_provider": "fixture-claude",
                "binary": claude,
                "version": "2.1.204 (Claude Code)",
                "sha256": sha256_file(claude)
            },
            "pi": {
                "policy_path": pi_policy,
                "policy_sha256": sha256_file(&pi_policy),
                "openshell_provider": "fixture-pi",
                "model_provider": "openai",
                "model": "openai/fixture-pi",
                "binary": claude,
                "version": "2.1.204 (Claude Code)",
                "sha256": sha256_file(claude)
            },
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

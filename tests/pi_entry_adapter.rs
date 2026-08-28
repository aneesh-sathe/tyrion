#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

struct RunningDaemon {
    child: Child,
    socket_path: PathBuf,
    _serial_guard: MutexGuard<'static, ()>,
}

static DAEMON_TEST_LOCK: Mutex<()> = Mutex::new(());

impl RunningDaemon {
    fn start(data_dir: &Path) -> Self {
        Self::start_with_arguments(data_dir, &[])
    }

    fn start_with_arguments(data_dir: &Path, extra_arguments: &[&str]) -> Self {
        let serial_guard = DAEMON_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut daemon = Self {
            child,
            socket_path,
            _serial_guard: serial_guard,
        };
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

struct RunningPi {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    error: BufReader<ChildStderr>,
}

impl RunningPi {
    fn start(socket_path: &Path) -> Self {
        Self::start_with_arguments(socket_path, &[])
    }

    fn start_with_arguments(socket_path: &Path, launcher_arguments: &[&str]) -> Self {
        Self::start_with_options(socket_path, launcher_arguments, None)
    }

    fn start_with_poll_interval(socket_path: &Path, interval_milliseconds: u64) -> Self {
        Self::start_with_options(socket_path, &[], Some(interval_milliseconds.to_string()))
    }

    fn start_with_options(
        socket_path: &Path,
        launcher_arguments: &[&str],
        poll_interval_milliseconds: Option<String>,
    ) -> Self {
        let fake_pi = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_pi_host.mjs");
        let mut command = Command::new(env!("CARGO_BIN_EXE_tyrion"));
        command
            .args(["--socket", path_text(socket_path), "pi", "--pi-command"])
            .arg(fake_pi)
            .args(launcher_arguments)
            .args(["--", "--mode", "rpc", "--no-session"]);
        if let Some(interval) = poll_interval_milliseconds {
            command.env("TYRION_PI_POLL_INTERVAL_MS", interval);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Pi Entry Session should launch");
        let input = child.stdin.take().expect("Pi stdin should be piped");
        let output = BufReader::new(child.stdout.take().expect("Pi stdout should be piped"));
        let error = BufReader::new(child.stderr.take().expect("Pi stderr should be piped"));
        Self {
            child,
            input,
            output,
            error,
        }
    }

    fn request(&mut self, request: Value) -> Value {
        let response = self.raw_request(request);
        assert_eq!(response["success"], true, "Pi request failed: {response}");
        response
    }

    fn raw_request(&mut self, request: Value) -> Value {
        serde_json::to_writer(&mut self.input, &request).expect("Pi request should serialize");
        self.input.write_all(b"\n").expect("Pi request should end");
        self.input.flush().expect("Pi request should flush");
        let expected_id = request["id"].as_str();
        let mut line = String::new();
        loop {
            line.clear();
            if self
                .output
                .read_line(&mut line)
                .expect("Pi output should read")
                == 0
            {
                let mut error = String::new();
                self.error.read_to_string(&mut error).unwrap();
                panic!("Pi exited before responding: {error}");
            }
            let response: Value = serde_json::from_str(&line).expect("Pi output should be JSON");
            if response["type"] == "response" && response["id"].as_str() == expected_id {
                return response;
            }
        }
    }

    fn prompt(&mut self, id: &str, message: &str) {
        self.request(json!({
            "id": id,
            "type": "prompt",
            "message": message,
        }));
    }

    fn messages(&mut self, id: &str) -> Vec<Value> {
        self.request(json!({
            "id": id,
            "type": "get_messages",
        }))["data"]["messages"]
            .as_array()
            .expect("Pi messages should be an array")
            .clone()
    }
}

impl Drop for RunningPi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        || stream.shutdown(Shutdown::Write).is_err()
    {
        return false;
    }
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .is_ok_and(|bytes| bytes > 0)
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

fn run_cli(socket_path: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(socket_path)])
        .args(arguments)
        .output()
        .expect("CLI should run")
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

fn run_pi_launcher(socket_path: &Path, pi_command: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args(["--socket", path_text(socket_path), "pi", "--pi-command"])
        .arg(pi_command)
        .args(["--", "--mode", "rpc"])
        .output()
        .expect("Pi launcher should run")
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn explicit_pi_launch_attaches_and_displays_full_mode() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start(&daemon.socket_path);

    pi.request(json!({
        "id": "status",
        "type": "prompt",
        "message": "/tyrion-status"
    }));
    let messages = pi.request(json!({
        "id": "messages",
        "type": "get_messages"
    }));
    let visible = &messages["data"]["messages"];
    assert!(visible.as_array().unwrap().iter().any(|message| {
        message["customType"] == "tyrion-commission"
            && message["content"].as_str().is_some_and(|content| {
                content.contains("Tyrion: Full") && content.contains("Attached")
            })
    }));
    assert!(visible.as_array().unwrap().iter().any(|message| {
        message["customType"] == "fake-pi-status" && message["content"] == "Tyrion: Full"
    }));
    let commands = pi.request(json!({"id": "commands", "type": "get_commands"}));
    let names = commands["data"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for command in [
        "tyrion-status",
        "tyrion-propose",
        "tyrion-accept",
        "tyrion-replay",
        "tyrion-steer",
        "tyrion-interrupt",
        "tyrion-retry",
        "tyrion-propose-operation",
        "tyrion-gate",
        "tyrion-pause",
        "tyrion-cancel",
        "tyrion-downgrade",
        "tyrion-takeover",
    ] {
        assert!(names.contains(&command), "Pi command {command} is missing");
    }
}

#[test]
fn pi_entry_cache_rejects_symlinked_directories() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    let redirect = temp.path().join("redirect");
    fs::create_dir(&redirect).unwrap();
    symlink(&redirect, temp.path().join("entry-adapters")).unwrap();
    let pi_command = temp.path().join("pi-exits");
    write_executable(&pi_command, "#!/bin/sh\nexit 0\n");

    let output = run_pi_launcher(&daemon.socket_path, &pi_command);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("adapter cache must be a user-owned regular directory"));
    assert_eq!(fs::read_dir(&redirect).unwrap().count(), 0);
}

#[test]
fn pi_entry_cache_rejects_a_socket_parent_other_users_can_replace() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let pi_command = temp.path().join("pi-exits");
    write_executable(&pi_command, "#!/bin/sh\nexit 0\n");

    let output = run_pi_launcher(&daemon.socket_path, &pi_command);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("socket parent chain that other users cannot modify"));
}

#[test]
fn pi_entry_cache_restores_private_permissions() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start(temp.path());
    let pi_command = temp.path().join("pi-exits");
    write_executable(&pi_command, "#!/bin/sh\nexit 0\n");

    let first = run_pi_launcher(&daemon.socket_path, &pi_command);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let directory = temp.path().join("entry-adapters");
    let extension = fs::read_dir(&directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&extension, fs::Permissions::from_mode(0o644)).unwrap();

    let second = run_pi_launcher(&daemon.socket_path, &pi_command);

    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::metadata(directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(extension).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn pi_completes_and_replays_a_commission_through_native_commands() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "return a greeting through Pi",
        "criteria": [{
            "id": "greeting",
            "description": "The Result contains the accepted greeting",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "return a greeting through Pi"}
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
    });

    pi.prompt(
        "propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    pi.prompt("accept", "/tyrion-accept");

    let deadline = Instant::now() + Duration::from_secs(5);
    let (commission, visible_content) = loop {
        pi.prompt("status", "/tyrion-status");
        let messages = pi.messages("status-messages");
        let message = messages
            .iter()
            .rev()
            .find(|message| message["customType"] == "tyrion-commission")
            .expect("Pi should render the Commission projection");
        let commission = message["details"]["commission"].clone();
        if commission["commission"]["status"] == "verified_complete" {
            break (commission, message["content"].as_str().unwrap().to_owned());
        }
        assert!(
            Instant::now() < deadline,
            "Commission did not complete through Pi: {commission}"
        );
        thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(commission["briefing"]["title"], "Verified Completion");
    assert_eq!(commission["criteria"][0]["status"], "passed");
    for required in [
        "Attachment: active",
        "Authority Envelope:",
        "deterministic.echo",
        "Resource ceilings:",
        "max_attempts\":1",
        "Workers",
        "Results",
        "Evidence",
        "Verification:",
        "Completion briefing:",
    ] {
        assert!(
            visible_content.contains(required),
            "Pi visible Commission summary omitted {required}: {visible_content}"
        );
    }
    pi.prompt("replay", "/tyrion-replay 0");
    let messages = pi.messages("replay-messages");
    let replay = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-events")
        .expect("Pi should render replayed durable events");
    let event_types = replay["details"]["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(event_types.starts_with(&[
        "commission_proposed",
        "attachment_joined",
        "active_attachment_changed",
        "commission_accepted",
    ]));
    assert_eq!(event_types.last(), Some(&"commission_verified_complete"));
}

#[test]
fn pi_capability_loss_is_visible_and_revokes_missing_controls() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start_with_arguments(&daemon.socket_path, &[]);
    let proposal = json!({
        "goal": "remain inert while Pi downgrades",
        "criteria": [{
            "id": "inert",
            "description": "The proposal remains unaccepted",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "remain inert while Pi downgrades"}
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
    });
    let encoded_proposal = serde_json::to_string(&proposal).unwrap();

    pi.prompt(
        "propose-before-downgrade",
        &format!("/tyrion-propose {encoded_proposal}"),
    );
    pi.prompt("downgrade", "/tyrion-downgrade observer");
    let messages = pi.messages("downgraded-messages");
    let downgraded = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .expect("Pi should render its downgraded Attachment");
    assert_eq!(downgraded["details"]["attachment"]["mode"], "observer");
    assert!(downgraded["content"]
        .as_str()
        .unwrap()
        .contains("Tyrion: Observer"));
    assert!(downgraded["content"]
        .as_str()
        .unwrap()
        .contains("This Entry Session cannot create Commission Proposals."));
    assert!(downgraded["details"]["attachment"]["missing_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|missing| missing["capability"] == "proposal_creation"));

    let denied = pi.raw_request(json!({
        "id": "propose-after-downgrade",
        "type": "prompt",
        "message": format!("/tyrion-propose {encoded_proposal}"),
    }));
    assert_eq!(denied["success"], false);
    assert!(denied["error"]
        .as_str()
        .unwrap()
        .contains("lacks the proposal_creation capability"));
}

#[test]
fn pi_explicitly_takes_control_back_from_another_harness() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "handoff one Commission",
        "criteria": [{
            "id": "handoff",
            "description": "The accepted Result survives handoff",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "handoff one Commission"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "handoff-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    let messages = pi.messages("handoff-proposal-messages");
    let proposal_projection = &messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["commission"];
    let commission_id = proposal_projection["commission"]["id"].as_str().unwrap();

    let issued = successful_json(run_cli(
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
            "issue-codex-handoff-token",
        ],
    ));
    let mut connect_arguments = vec![
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
        "codex-handoff-session",
        "--commission-id",
        commission_id,
        "--idempotency-key",
        "connect-codex-handoff",
    ];
    for capability in [
        "proposal_creation",
        "commission_acceptance",
        "commission_inspection",
        "event_replay",
        "control_takeover",
        "material_notifications",
        "persistent_mode_display",
        "worker_steering",
        "worker_interruption",
    ] {
        connect_arguments.extend(["--capability", capability]);
    }
    let codex = successful_json(run_cli(&daemon.socket_path, &connect_arguments));
    let codex_token = codex["attachment_session_token"].as_str().unwrap();
    successful_json(run_cli(
        &daemon.socket_path,
        &[
            "--attachment-token",
            codex_token,
            "commission",
            "take-control",
            commission_id,
            "--expected-revision",
            "0",
            "--expected-control-revision",
            "0",
            "--idempotency-key",
            "codex-takes-control",
        ],
    ));

    pi.prompt("pi-observes-handoff", "/tyrion-status");
    let messages = pi.messages("pi-observer-messages");
    let observer = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap();
    assert!(observer["content"]
        .as_str()
        .unwrap()
        .contains("Attachment: observer"));

    pi.prompt("pi-takes-control", "/tyrion-takeover");
    pi.prompt("pi-accepts-after-handoff", "/tyrion-accept");
    pi.prompt("pi-replays-handoffs", "/tyrion-replay 0");
    let messages = pi.messages("pi-handoff-messages");
    let replay = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-events")
        .unwrap();
    let handoffs = replay["details"]["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "active_attachment_changed")
        .collect::<Vec<_>>();
    assert_eq!(handoffs.len(), 3);
    let sequences = replay["details"]["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert_eq!(
        sequences
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        sequences.len()
    );
    assert_ne!(
        handoffs[1]["payload"]["active_attachment_id"],
        handoffs[2]["payload"]["active_attachment_id"]
    );
    assert_eq!(
        handoffs[2]["payload"]["active_attachment_id"],
        proposal_projection["attachments"][0]["id"]
    );
}

#[test]
fn pi_pauses_and_cancels_without_losing_durable_state() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-defer-ready-dispatch"]);
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "pause a Commission from Pi",
        "criteria": [{
            "id": "paused",
            "description": "Work runs only while dispatch is enabled",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "pause a Commission from Pi"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "pause-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    pi.prompt("pause-accept", "/tyrion-accept");
    pi.prompt("pause", "/tyrion-pause");
    pi.prompt("cancel", "/tyrion-cancel");
    pi.prompt("cancelled-status", "/tyrion-status");
    pi.prompt("cancelled-replay", "/tyrion-replay 0");

    let messages = pi.messages("cancelled-messages");
    let projection = &messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["commission"];
    assert_eq!(projection["commission"]["status"], "cancelled");
    assert_eq!(projection["recovery"]["state"], "cancelled");
    assert_eq!(
        projection["recovery"]["cancellation"]["rollback_claimed"],
        false
    );
    assert_eq!(projection["assignments"][0]["status"], "cancelled");
    let replay = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-events")
        .unwrap();
    let event_types = replay["details"]["replay"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"commission_paused"));
    assert_eq!(event_types.last(), Some(&"commission_cancelled"));
}

#[test]
fn pi_renders_an_actionable_blocker_decision_first() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "surface a missing Worker capability",
        "worker_requirements": {
            "capabilities": ["pi-blocker-fixture"],
            "tools": [], "skills": [], "min_context_tokens": 0,
            "assignment_constraints": []
        },
        "criteria": [{
            "id": "blocked",
            "description": "A qualified Worker is required",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "surface a missing Worker capability"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "blocker-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    pi.prompt("blocker-accept", "/tyrion-accept");
    pi.prompt("blocker-status", "/tyrion-status");

    let messages = pi.messages("blocker-messages");
    let rendered = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap();
    let requirement = rendered["details"]["commission"]["recovery"]["resumable_blocker"]
        ["exact_next_requirement"]
        .as_str()
        .expect("Blocker should include the exact next requirement");
    let content = rendered["content"].as_str().unwrap();
    assert!(content.lines().nth(1).unwrap().starts_with("BLOCKER:"));
    assert!(content.contains(requirement));
    assert!(content.contains("Passed criteria:"));
    assert!(content.contains("Unresolved criteria: blocked"));
}

#[test]
fn pi_opens_and_inspects_an_approval_gate_without_approval_authority() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let repository = temp.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    std::fs::write(repository.join("README.md"), "before\n").unwrap();
    let daemon =
        RunningDaemon::start_with_arguments(temp.path(), &["--fault-hold-worker-for-control"]);
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "hold work while Pi opens an Approval Gate",
        "plan": {"assignments": [{
            "id": "held-worker",
            "goal": "hold work while Pi opens an Approval Gate",
            "dependencies": [],
            "criterion_ids": ["gated"],
            "purpose": "critical_path",
            "read_scopes": [],
            "write_scopes": [],
            "resources": {
                "concurrency_slots": 1, "max_storage_bytes": 1048576,
                "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
            }
        }, {
            "id": "later-worker",
            "goal": "finish after the Approval Gate fixture",
            "dependencies": ["held-worker"],
            "criterion_ids": ["later"],
            "purpose": "critical_path",
            "read_scopes": [],
            "write_scopes": [],
            "resources": {
                "concurrency_slots": 1, "max_storage_bytes": 1048576,
                "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
            }
        }]},
        "criteria": [
            {
                "id": "gated",
                "description": "The held Result remains subject to verification",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier": {
                    "kind": "exact_match",
                    "expected": "hold work while Pi opens an Approval Gate"
                }
            }, {
                "id": "later",
                "description": "Dependent work remains blocked",
                "required_evidence": "exact_output",
                "verifier_type": "deterministic",
                "verification_depth": "standard",
                "verifier": {
                    "kind": "exact_match",
                    "expected": "finish after the Approval Gate fixture"
                }
            }
        ],
        "authority": {
            "repositories": [path_text(&repository)],
            "paths": ["README.md"],
            "actions": ["deterministic.echo", "filesystem.write"],
            "destinations": ["local"],
            "effects": ["filesystem.write"]
        },
        "resource_ceilings": {
            "max_attempts": 2, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "gate-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    pi.prompt("gate-accept", "/tyrion-accept");

    let deadline = Instant::now() + Duration::from_secs(5);
    let running = loop {
        pi.prompt("gate-status", "/tyrion-status");
        let messages = pi.messages("gate-status-messages");
        let projection = messages
            .iter()
            .rev()
            .find(|message| message["customType"] == "tyrion-commission")
            .unwrap()["details"]["commission"]
            .clone();
        if projection["attempts"][0]["status"] == "running" {
            break projection;
        }
        assert!(
            Instant::now() < deadline,
            "Pi Worker did not start: {projection}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let operation = json!({
        "assignment_id": running["attempts"][0]["assignment_id"],
        "attempt_id": running["attempts"][0]["id"],
        "worker_lease_id": running["attempts"][0]["lease"]["id"],
        "mandate_revision": 1,
        "plan_revision": 1,
        "operation": "filesystem.write",
        "repository": path_text(&repository),
        "target": "README.md",
        "parameters": {"content": "after\n"},
        "destination": "local",
        "effect": "filesystem.write",
        "consequences": ["Replace the current README content"],
        "limits": {"max_output_bytes": 1024, "max_duration_seconds": 5}
    });
    pi.prompt(
        "gate-operation",
        &format!(
            "/tyrion-propose-operation {}",
            serde_json::to_string(&operation).unwrap()
        ),
    );
    let messages = pi.messages("gate-operation-messages");
    let rendered = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap();
    let gate = &rendered["details"]["commission"]["approval_gates"][0];
    assert_eq!(gate["status"], "open");
    assert!(rendered["content"]
        .as_str()
        .unwrap()
        .contains("APPROVAL REQUIRED"));
    assert_eq!(
        std::fs::read_to_string(repository.join("README.md")).unwrap(),
        "before\n"
    );

    pi.prompt(
        "gate-inspect",
        &format!("/tyrion-gate {}", gate["id"].as_str().unwrap()),
    );
    let messages = pi.messages("gate-inspect-messages");
    let inspected = messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-approval-gate")
        .unwrap();
    assert_eq!(inspected["details"]["gate"]["id"], gate["id"]);
    assert!(inspected["content"]
        .as_str()
        .unwrap()
        .contains("independent Principal control path"));
}

#[test]
fn pi_surfaces_material_events_without_manual_polling() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "notify Pi when work completes",
        "criteria": [{
            "id": "notified",
            "description": "The Result completes without manual polling",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "notify Pi when work completes"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "notification-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    pi.prompt("notification-accept", "/tyrion-accept");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = pi.messages("notification-messages");
        if messages.iter().any(|message| {
            message["customType"] == "tyrion-notification"
                && message["details"]["event"]["type"] == "commission_verified_complete"
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Pi did not surface material completion without a manual replay"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn delayed_incremental_replay_cannot_regress_the_pi_cursor() {
    let temp = TempDir::new().unwrap();
    let daemon = RunningDaemon::start_with_arguments(
        temp.path(),
        &["--fault-hold-incremental-replay-milliseconds", "1500"],
    );
    let mut pi = RunningPi::start_with_poll_interval(&daemon.socket_path, 2000);
    let proposal = json!({
        "goal": "preserve a monotonic Pi replay cursor",
        "criteria": [{
            "id": "cursor",
            "description": "The Pi cursor never moves backwards",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "preserve a monotonic Pi replay cursor"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    pi.prompt(
        "cursor-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    let proposed = pi.messages("cursor-proposed-messages");
    let proposed_cursor = proposed
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["event_cursor"]
        .as_u64()
        .unwrap();

    pi.prompt("cursor-accept", "/tyrion-accept");
    let accepted = pi.messages("cursor-accepted-messages");
    let accepted_cursor = accepted
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["event_cursor"]
        .as_u64()
        .unwrap();
    assert!(accepted_cursor > proposed_cursor);

    // The first incremental poll snapshots the completed Commission at 2 seconds,
    // then the daemon holds its response. An explicit replay advances the cursor
    // and surfaces the completion while that older response remains in flight.
    thread::sleep(Duration::from_millis(2200));
    pi.prompt("cursor-replay", "/tyrion-replay 0");
    let replayed = pi.messages("cursor-replay-messages");
    let events = replayed
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-events")
        .unwrap()["details"]["replay"]["events"]
        .as_array()
        .unwrap();
    let completion_sequence = events
        .iter()
        .find(|event| event["type"] == "commission_verified_complete")
        .expect("explicit replay should include deterministic completion")["sequence"]
        .as_u64()
        .unwrap();
    let sequences = events
        .iter()
        .map(|event| event["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert_eq!(
        sequences
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        sequences.len()
    );

    thread::sleep(Duration::from_millis(1600));
    pi.prompt("cursor-status", "/tyrion-status");
    let status = pi.messages("cursor-status-messages");
    let visible_cursor = status
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["event_cursor"]
        .as_u64()
        .unwrap();
    assert_eq!(visible_cursor, *sequences.last().unwrap());
    assert_eq!(
        status
            .iter()
            .filter(|message| {
                message["customType"] == "tyrion-notification"
                    && message["details"]["event"]["sequence"].as_u64() == Some(completion_sequence)
            })
            .count(),
        1,
        "the delayed stale replay must not duplicate the visible material event"
    );
}

#[test]
fn a_new_pi_session_replays_work_completed_while_disconnected() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let daemon = RunningDaemon::start(temp.path());
    let mut first_pi = RunningPi::start(&daemon.socket_path);
    let proposal = json!({
        "goal": "complete while Pi is disconnected",
        "criteria": [{
            "id": "durable",
            "description": "The daemon completes independently of Pi",
            "required_evidence": "exact_output",
            "verifier_type": "deterministic",
            "verification_depth": "standard",
            "verifier": {"kind": "exact_match", "expected": "complete while Pi is disconnected"}
        }],
        "authority": {
            "repositories": [], "paths": [], "actions": ["deterministic.echo"],
            "destinations": [], "effects": []
        },
        "resource_ceilings": {
            "max_attempts": 1, "max_elapsed_seconds": 30,
            "max_worker_concurrency": 1, "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0, "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    });
    first_pi.prompt(
        "disconnect-propose",
        &format!(
            "/tyrion-propose {}",
            serde_json::to_string(&proposal).unwrap()
        ),
    );
    let messages = first_pi.messages("disconnect-proposal-messages");
    let projection = &messages
        .iter()
        .rev()
        .find(|message| message["customType"] == "tyrion-commission")
        .unwrap()["details"]["commission"];
    let commission_id = projection["commission"]["id"].as_str().unwrap().to_owned();
    let cursor = projection["events"].as_array().unwrap().last().unwrap()["sequence"]
        .as_i64()
        .unwrap()
        .to_string();
    first_pi.prompt("disconnect-accept", "/tyrion-accept");
    drop(first_pi);

    let mut resumed_pi = RunningPi::start_with_arguments(
        &daemon.socket_path,
        &[
            "--commission-id",
            &commission_id,
            "--last-event-sequence",
            &cursor,
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = resumed_pi.messages("resumed-messages");
        let replayed_completion = messages.iter().any(|message| {
            message["customType"] == "tyrion-events"
                && message["details"]["replay"]["events"]
                    .as_array()
                    .is_some_and(|events| {
                        events
                            .iter()
                            .any(|event| event["type"] == "commission_verified_complete")
                    })
        });
        let completed_projection = messages.iter().any(|message| {
            message["customType"] == "tyrion-commission"
                && message["details"]["commission"]["commission"]["status"] == "verified_complete"
        });
        if replayed_completion && completed_projection {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reconnected Pi did not replay disconnected completion"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

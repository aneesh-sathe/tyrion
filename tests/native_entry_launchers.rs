#![cfg(unix)]

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

use fs2::FileExt;

#[test]
fn tyrion_claude_starts_a_local_daemon_and_launches_the_native_tui_at_the_git_root() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let project = temp.path().join("project");
    let nested = project.join("src/nested");
    let binaries = temp.path().join("bin");
    let data_dir = temp.path().join("state");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&binaries).unwrap();
    write_executable(
        &binaries.join("claude"),
        r#"#!/bin/sh
set -eu
test -S "$TYRION_SOCKET"
printf '%s\n' "$PWD" "$TYRION_SOCKET" "$TYRION_PROJECT_ROOT"
for argument in "$@"; do
    printf 'ARG=%s\n' "$argument"
done
"#,
    );
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(binaries.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .current_dir(&nested)
        .env("PATH", path)
        .env("TYRION_DATA_DIR", &data_dir)
        .args(["claude", "--", "--model", "opus"])
        .output()
        .expect("Tyrion launcher should run");
    let canonical_project = fs::canonicalize(&project).unwrap();

    assert!(
        output.status.success(),
        "launcher failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], canonical_project.to_str().unwrap());
    assert_eq!(lines[1], data_dir.join("tyrion.sock").to_str().unwrap());
    assert_eq!(lines[2], canonical_project.to_str().unwrap());
    let arguments = captured_arguments(&lines);
    assert_eq!(&arguments[0..2], ["--mcp-config", arguments[1]]);
    let mcp_config: Value = serde_json::from_str(arguments[1]).unwrap();
    assert_eq!(
        mcp_config["mcpServers"]["tyrion"]["command"],
        env!("CARGO_BIN_EXE_tyrion")
    );
    assert_eq!(
        mcp_config["mcpServers"]["tyrion"]["args"],
        json!([
            "--socket",
            data_dir.join("tyrion.sock"),
            "entry-mcp",
            "--harness",
            "claude"
        ])
    );
    assert_eq!(arguments[2], "--append-system-prompt");
    assert!(arguments[3].contains("tyrion_start_commission"));
    assert_eq!(&arguments[4..], ["--model", "opus"]);
    assert!(data_dir.join("state.sqlite3").is_file());
    assert_eq!(
        fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!daemon_responds(&data_dir.join("tyrion.sock")));
}

#[test]
fn tyrion_codex_starts_a_local_daemon_and_launches_the_native_tui_at_the_git_root() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let project = temp.path().join("project");
    let nested = project.join("src/nested");
    let binaries = temp.path().join("bin");
    let data_dir = temp.path().join("state");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&binaries).unwrap();
    write_executable(
        &binaries.join("codex"),
        r#"#!/bin/sh
set -eu
test -S "$TYRION_SOCKET"
printf '%s\n' "$PWD" "$TYRION_SOCKET" "$TYRION_PROJECT_ROOT"
for argument in "$@"; do
    printf 'ARG=%s\n' "$argument"
done
"#,
    );
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(binaries.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .current_dir(&nested)
        .env("PATH", path)
        .env("TYRION_DATA_DIR", &data_dir)
        .args(["codex", "--", "--model", "gpt-5.5"])
        .output()
        .expect("Tyrion launcher should run");
    let canonical_project = fs::canonicalize(&project).unwrap();

    assert!(
        output.status.success(),
        "launcher failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], canonical_project.to_str().unwrap());
    assert_eq!(lines[1], data_dir.join("tyrion.sock").to_str().unwrap());
    assert_eq!(lines[2], canonical_project.to_str().unwrap());
    let arguments = captured_arguments(&lines);
    assert_eq!(arguments[0], "-c");
    assert!(arguments[1].starts_with("mcp_servers.tyrion.command="));
    assert!(arguments[1].contains(env!("CARGO_BIN_EXE_tyrion")));
    assert_eq!(arguments[2], "-c");
    assert!(arguments[3].starts_with("mcp_servers.tyrion.args="));
    assert!(arguments[3].contains(data_dir.join("tyrion.sock").to_str().unwrap()));
    assert!(arguments[3].contains("entry-mcp"));
    assert!(arguments[3].contains("codex"));
    assert_eq!(
        &arguments[4..8],
        [
            "-c",
            "mcp_servers.tyrion.required=true",
            "-c",
            "mcp_servers.tyrion.default_tools_approval_mode=\"auto\""
        ]
    );
    assert_eq!(arguments[8], "-c");
    assert!(arguments[9].starts_with("developer_instructions="));
    assert!(arguments[9].contains("tyrion_start_commission"));
    assert_eq!(&arguments[10..], ["--model", "gpt-5.5"]);
    assert!(data_dir.join("state.sqlite3").is_file());
    assert_eq!(
        fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!daemon_responds(&data_dir.join("tyrion.sock")));
}

#[test]
fn native_entry_launcher_makes_no_changes_outside_a_git_repository() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let binaries = temp.path().join("bin");
    let data_dir = temp.path().join("state");
    let launch_marker = temp.path().join("harness-launched");
    fs::create_dir_all(&binaries).unwrap();
    write_executable(
        &binaries.join("claude"),
        &format!("#!/bin/sh\ntouch '{}'\n", launch_marker.to_str().unwrap()),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .current_dir(temp.path())
        .env("PATH", &binaries)
        .env("TYRION_DATA_DIR", &data_dir)
        .arg("claude")
        .output()
        .expect("Tyrion launcher should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("tyrion claude must be launched inside a Git repository"));
    assert!(!launch_marker.exists());
    assert!(!data_dir.exists());
}

#[test]
fn auto_managed_daemon_allows_only_one_native_entry_session() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let project = temp.path().join("project");
    let binaries = temp.path().join("bin");
    let data_dir = temp.path().join("state");
    let launch_marker = temp.path().join("harness-launched");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(&binaries).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    write_executable(
        &binaries.join("claude"),
        &format!("#!/bin/sh\ntouch '{}'\n", launch_marker.to_str().unwrap()),
    );
    let session_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join("native-entry.lock"))
        .unwrap();
    session_lock.try_lock_exclusive().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .current_dir(&project)
        .env("PATH", &binaries)
        .env("TYRION_DATA_DIR", &data_dir)
        .arg("claude")
        .output()
        .expect("Tyrion launcher should run");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("another auto-managed native Entry Session is already running"));
    assert!(!launch_marker.exists());
    assert!(!data_dir.join("state.sqlite3").exists());
}

#[test]
fn native_entry_mcp_runs_sequential_commissions_with_minimal_lifecycle_tools() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let mut daemon = RunningDaemon::start(temp.path());
    let mut entry = Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .args([
            "entry-mcp",
            "--socket",
            daemon.socket_path.to_str().unwrap(),
            "--harness",
            "claude",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Entry MCP server should launch");
    let mut input = entry.stdin.take().unwrap();
    let mut output = BufReader::new(entry.stdout.take().unwrap());

    let initialized = mcp_request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "launcher-test", "version": "1.0.0"}
            }
        }),
    );
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialized["result"]["capabilities"], json!({"tools": {}}));
    writeln!(
        input,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    input.flush().unwrap();

    let listed = mcp_request(
        &mut input,
        &mut output,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "tyrion_start_commission",
            "tyrion_status",
            "tyrion_cancel_commission"
        ]
    );
    let start_tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "tyrion_start_commission")
        .unwrap();
    assert_eq!(
        start_tool["inputSchema"]["properties"]["proposal"]["required"],
        json!([
            "goal",
            "execution",
            "criteria",
            "authority",
            "resource_ceilings",
            "known_uncertainties"
        ])
    );
    assert_eq!(
        start_tool["inputSchema"]["properties"]["proposal"]["properties"]["execution"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        start_tool["inputSchema"]["properties"]["proposal"]["properties"]["resource_ceilings"]
            ["properties"]["max_model_spend_cents"]["maximum"],
        100
    );

    let first = start_commission(
        &mut input,
        &mut output,
        3,
        "return the first native greeting",
    );
    assert_eq!(first["result"]["isError"], false);
    let first_id = first["result"]["structuredContent"]["commission"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for_current_commission(&mut input, &mut output, 4, "verified_complete");
    daemon.restart();
    wait_for_current_commission(&mut input, &mut output, 50, "verified_complete");
    let duplicate = start_commission(
        &mut input,
        &mut output,
        51,
        "return the first native greeting",
    );
    assert_eq!(duplicate["result"]["isError"], false);
    assert_eq!(
        duplicate["result"]["structuredContent"]["commission"]["id"],
        first_id
    );

    let second = start_commission(
        &mut input,
        &mut output,
        100,
        "return the second native greeting",
    );
    assert_eq!(second["result"]["isError"], false);
    let second_id = second["result"]["structuredContent"]["commission"]["id"]
        .as_str()
        .unwrap();
    assert_ne!(first_id, second_id);
    wait_for_current_commission(&mut input, &mut output, 101, "verified_complete");

    let mut oversized = deterministic_proposal("try an oversized native Commission");
    oversized["resource_ceilings"]["max_model_spend_cents"] = json!(101);
    let rejected = mcp_request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "tools/call",
            "params": {
                "name": "tyrion_start_commission",
                "arguments": {"proposal": oversized}
            }
        }),
    );
    assert_eq!(rejected["result"]["isError"], true);
    assert!(rejected["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("max_model_spend_cents"));

    let mut failing = deterministic_proposal("produce a deliberately failing result");
    failing["criteria"][0]["verifier"]["expected"] = json!("a different result");
    let active = mcp_request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0",
            "id": 201,
            "method": "tools/call",
            "params": {
                "name": "tyrion_start_commission",
                "arguments": {"proposal": failing}
            }
        }),
    );
    assert_eq!(active["result"]["isError"], false);
    let overlapping = start_commission(
        &mut input,
        &mut output,
        202,
        "do not start while another Commission is active",
    );
    assert_eq!(overlapping["result"]["isError"], true);
    assert!(overlapping["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("already has a non-terminal Commission"));
    let cancelled = mcp_request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0",
            "id": 203,
            "method": "tools/call",
            "params": {"name": "tyrion_cancel_commission", "arguments": {}}
        }),
    );
    assert_eq!(cancelled["result"]["isError"], false);
    assert_eq!(
        cancelled["result"]["structuredContent"]["commission"]["status"],
        "cancelled"
    );
    let after_cancel = start_commission(
        &mut input,
        &mut output,
        204,
        "start fresh after cancelling blocked work",
    );
    assert_eq!(after_cancel["result"]["isError"], false);
    wait_for_current_commission(&mut input, &mut output, 205, "verified_complete");

    drop(input);
    assert!(entry.wait().unwrap().success());
}

fn start_commission(
    input: &mut impl Write,
    output: &mut impl BufRead,
    id: u64,
    goal: &str,
) -> Value {
    mcp_request(
        input,
        output,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "tyrion_start_commission",
                "arguments": {"proposal": deterministic_proposal(goal)}
            }
        }),
    )
}

fn wait_for_current_commission(
    input: &mut impl Write,
    output: &mut impl BufRead,
    mut id: u64,
    expected_status: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inspected = mcp_request(
            input,
            output,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": "tyrion_status", "arguments": {}}
            }),
        );
        if inspected["result"]["structuredContent"]["commission"]["status"] == expected_status {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Commission did not reach {expected_status}: {inspected}"
        );
        id += 1;
        thread::sleep(Duration::from_millis(20));
    }
}

fn deterministic_proposal(goal: &str) -> Value {
    json!({
        "goal": goal,
        "execution": {"kind": "deterministic"},
        "criteria": [{
            "id": "greeting",
            "description": "The Result contains the requested greeting",
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
            "max_storage_bytes": 1048576,
            "max_model_spend_cents": 0,
            "max_paid_service_spend_cents": 0
        },
        "known_uncertainties": []
    })
}

struct RunningDaemon {
    child: Child,
    data_dir: std::path::PathBuf,
    socket_path: std::path::PathBuf,
}

impl RunningDaemon {
    fn start(data_dir: &Path) -> Self {
        let socket_path = data_dir.join("tyrion.sock");
        let child = spawn_daemon(data_dir, &socket_path);
        let mut daemon = Self {
            child,
            data_dir: data_dir.to_path_buf(),
            socket_path,
        };
        daemon.wait_until_ready();
        daemon
    }

    fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = spawn_daemon(&self.data_dir, &self.socket_path);
        self.wait_until_ready();
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if daemon_responds(&self.socket_path) {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("daemon exited before becoming ready: {status}");
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon did not become ready");
    }
}

fn daemon_responds(socket_path: &Path) -> bool {
    Command::new(env!("CARGO_BIN_EXE_tyrion"))
        .arg("--socket")
        .arg(socket_path)
        .args(["commission", "inspect", "native-entry-readiness-probe"])
        .output()
        .is_ok_and(|output| output.status.code() == Some(2))
}

fn spawn_daemon(data_dir: &Path, socket_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tyriond"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--socket")
        .arg(socket_path)
        .spawn()
        .expect("daemon should start")
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mcp_request(input: &mut impl Write, output: &mut impl BufRead, request: Value) -> Value {
    writeln!(input, "{request}").unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    assert_ne!(output.read_line(&mut line).unwrap(), 0);
    serde_json::from_str(&line).unwrap()
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn captured_arguments<'a>(lines: &'a [&str]) -> Vec<&'a str> {
    lines[3..]
        .iter()
        .map(|line| line.strip_prefix("ARG=").unwrap())
        .collect()
}

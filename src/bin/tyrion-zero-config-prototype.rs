//! PROTOTYPE ONLY: drives the real `tyrion codex` launcher against disposable
//! Entry and Worker fixtures so the zero-configuration orchestration seam can
//! be exercised without a model charge or persistent harness configuration.

#[path = "../prototype_zero_config_codex.rs"]
mod prototype_state;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use prototype_state::{select_runtime, Action, PrototypeState, RuntimeFacts};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const EXPECTED_CODEX: &str = "codex-cli 0.147.0";

fn main() {
    if invoked_as_codex() {
        if let Err(error) = run_fixture_entry() {
            eprintln!("prototype Entry failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run_prototype() {
        eprintln!("prototype failed: {error}");
        std::process::exit(1);
    }
}

fn run_prototype() -> Result<(), String> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scratch_parent = repository.join(".scratch");
    fs::create_dir_all(&scratch_parent).map_err(display_error)?;
    let scratch = ScratchRoot::new(&scratch_parent)?;
    let data_dir = scratch.path.join("state");
    fs::create_dir(&data_dir).map_err(display_error)?;
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).map_err(display_error)?;

    let production_runtime = inspect_production_runtime();
    let mut state = PrototypeState::new();
    render(&state);

    build_product_binaries(&repository)?;
    let current_executable = env::current_exe().map_err(display_error)?;
    let binary_dir = current_executable
        .parent()
        .ok_or_else(|| "prototype executable has no parent directory".to_owned())?;
    let tyrion = binary_dir.join("tyrion");
    let tyriond = binary_dir.join("tyriond");
    require_regular_file(&tyrion, "tyrion binary")?;
    require_regular_file(&tyriond, "tyriond binary")?;

    let project = create_principal_repository(&scratch.path)?;
    let fixture_bin = scratch.path.join("bin");
    fs::create_dir(&fixture_bin).map_err(display_error)?;
    symlink(&current_executable, fixture_bin.join("codex")).map_err(display_error)?;
    let docker = write_executable(
        &fixture_bin.join("docker-worker-fixture"),
        include_str!("../../tests/fixtures/fake_docker.sh"),
    )?;
    let worker_codex = write_executable(
        &fixture_bin.join("codex-worker-fixture"),
        include_str!("../../tests/fixtures/fake_codex.sh"),
    )?;
    fs::create_dir(fixture_bin.join("fake-docker")).map_err(display_error)?;
    let runtime = write_runtime_fixture(&repository, &data_dir, &docker, &worker_codex)?;
    state.apply(Action::RuntimePrepared { production_runtime })?;
    render(&state);

    let inherited_path = env::var_os("PATH").unwrap_or_default();
    let fixture_path = env::join_paths(
        std::iter::once(fixture_bin.clone()).chain(env::split_paths(&inherited_path)),
    )
    .map_err(display_error)?;
    let result_path = scratch.path.join("result.json");
    let mut command = Command::new(&tyrion);
    command
        .arg("codex")
        .current_dir(&project)
        .env("PATH", fixture_path)
        .env("TYRION_DATA_DIR", &data_dir)
        .env("TYRION_PROTOTYPE_ZERO_CONFIG_CODEX", "1")
        .env("TYRION_PROTOTYPE_CODEX_WORKER_CONFIG", &runtime)
        .env("TYRION_PROTOTYPE_TYRION_BIN", &tyrion)
        .env("TYRION_PROTOTYPE_RESULT", &result_path)
        .process_group(0);
    let mut child = command.spawn().map_err(display_error)?;
    let process_group = ChildProcessGroup::new(child.id());
    let status = child.wait().map_err(display_error)?;
    process_group.terminate();
    if !status.success() {
        return Err(format!("tyrion codex exited with {status}"));
    }

    let result: Value = serde_json::from_slice(&fs::read(&result_path).map_err(display_error)?)
        .map_err(display_error)?;
    let commission_id = required_text(&result, "/commission/id")?;
    let status = required_text(&result, "/commission/status")?;
    let integration_revision = required_text(&result, "/commission/artifact_revision")?;
    let principal_changed = project.join("issue-4.txt").exists();
    let integration_file = data_dir
        .join("integrations")
        .join(&commission_id)
        .join("repository/issue-4.txt");
    if fs::read_to_string(&integration_file).map_err(display_error)? != "contained codex result\n" {
        return Err("integrated artifact did not contain the expected result".into());
    }
    let log =
        fs::read_to_string(fixture_bin.join("fake-docker/commands.log")).map_err(display_error)?;
    let created = log.matches("sandbox create").count();
    let deleted = log.matches("sandbox delete").count();
    if status != "verified_complete" || principal_changed || created != 3 || deleted != 3 {
        return Err(format!(
            "unexpected verdict: status={status}, principal_changed={principal_changed}, sandboxes={created}/{deleted}"
        ));
    }
    state.apply(Action::EntryAttached)?;
    state.apply(Action::CommissionAccepted {
        commission_id: commission_id.clone(),
    })?;
    state.apply(Action::CommissionVerified {
        status,
        integration_revision,
        principal_checkout_mutated: principal_changed,
        sandboxes_created: created,
        sandboxes_deleted: deleted,
    })?;
    render(&state);
    println!(
        "\nVERDICT: zero-config orchestration is viable when Tyrion owns the runtime bundle; this machine still lacks a provisioned pinned Worker image, so containment is not attested."
    );
    Ok(())
}

fn run_fixture_entry() -> Result<(), String> {
    let socket = PathBuf::from(required_env("TYRION_SOCKET")?);
    let project = PathBuf::from(required_env("TYRION_PROJECT_ROOT")?);
    let tyrion = PathBuf::from(required_env("TYRION_PROTOTYPE_TYRION_BIN")?);
    let result_path = PathBuf::from(required_env("TYRION_PROTOTYPE_RESULT")?);
    validate_injected_codex_arguments()?;

    let mut state = PrototypeState::new();
    state.apply(Action::RuntimePrepared {
        production_runtime: select_runtime(RuntimeFacts {
            tyrion_owned_bundle: true,
            boundary_attested: false,
            docker_version: Some("Docker version 28.0.4".into()),
            guest_codex_version: Some(EXPECTED_CODEX.into()),
            ambient_codex_version: None,
        })
        .summary(),
    })?;
    let mut mcp = Command::new(tyrion)
        .arg("--socket")
        .arg(&socket)
        .arg("entry-mcp")
        .args(["--harness", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(display_error)?;
    let mut input = mcp
        .stdin
        .take()
        .ok_or_else(|| "Entry MCP stdin was unavailable".to_owned())?;
    let output = mcp
        .stdout
        .take()
        .ok_or_else(|| "Entry MCP stdout was unavailable".to_owned())?;
    let mut output = BufReader::new(output);
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
                "clientInfo": {"name": "zero-config-prototype", "version": "0.1.0"}
            }
        }),
    )?;
    if initialized
        .pointer("/result/serverInfo/name")
        .and_then(Value::as_str)
        != Some("tyrion-native-entry")
    {
        return Err("native Tyrion MCP server did not initialize".into());
    }
    writeln!(
        input,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .map_err(display_error)?;
    input.flush().map_err(display_error)?;
    state.apply(Action::EntryAttached)?;
    render(&state);

    let base_revision = git_text(&project, &["rev-parse", "HEAD"])?;
    let proposal = json!({
        "goal": "Add issue-4.txt containing contained codex result.",
        "execution": {
            "kind": "codex_git",
            "repository": project,
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
            "repositories": [project],
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
    });
    let accepted = mcp_request(
        &mut input,
        &mut output,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "tyrion_start_commission",
                "arguments": {"proposal": proposal}
            }
        }),
    )?;
    ensure_tool_success(&accepted)?;
    let commission_id = required_text(&accepted, "/result/structuredContent/commission/id")?;
    state.apply(Action::CommissionAccepted {
        commission_id: commission_id.clone(),
    })?;
    render(&state);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut request_id = 3;
    let completed = loop {
        let inspected = mcp_request(
            &mut input,
            &mut output,
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": "tyrion_status", "arguments": {}}
            }),
        )?;
        ensure_tool_success(&inspected)?;
        let status = required_text(&inspected, "/result/structuredContent/commission/status")?;
        if status == "verified_complete" {
            break inspected["result"]["structuredContent"].clone();
        }
        if matches!(status.as_str(), "blocked" | "cancelled") {
            return Err(format!("Commission became {status}: {inspected}"));
        }
        if Instant::now() >= deadline {
            return Err(format!("Commission did not complete: {inspected}"));
        }
        request_id += 1;
        thread::sleep(Duration::from_millis(25));
    };
    fs::write(
        result_path,
        serde_json::to_vec_pretty(&completed).map_err(display_error)?,
    )
    .map_err(display_error)?;
    drop(input);
    wait_for_child(&mut mcp, Duration::from_secs(2))?;
    Ok(())
}

fn validate_injected_codex_arguments() -> Result<(), String> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let joined = arguments.join("\n");
    for required in [
        "mcp_servers.tyrion.command=",
        "mcp_servers.tyrion.args=",
        "mcp_servers.tyrion.required=true",
        "developer_instructions=",
    ] {
        if !joined.contains(required) {
            return Err(format!("native launcher omitted {required}"));
        }
    }
    Ok(())
}

fn inspect_production_runtime() -> String {
    select_runtime(RuntimeFacts {
        tyrion_owned_bundle: false,
        boundary_attested: false,
        docker_version: command_version("docker"),
        guest_codex_version: None,
        ambient_codex_version: command_version("codex"),
    })
    .summary()
}

fn command_version(name: &str) -> Option<String> {
    let output = Command::new(name).arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn build_product_binaries(repository: &Path) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(repository)
        .args(["build", "--quiet", "--bin", "tyrion", "--bin", "tyriond"])
        .status()
        .map_err(display_error)?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("cargo build exited with {status}"))
}

fn create_principal_repository(root: &Path) -> Result<PathBuf, String> {
    let project = root.join("principal-checkout");
    fs::create_dir(&project).map_err(display_error)?;
    git(&project, &["init", "-q"])?;
    git(&project, &["config", "user.name", "Tyrion Prototype"])?;
    git(
        &project,
        &["config", "user.email", "prototype@tyrion.invalid"],
    )?;
    fs::write(project.join("README.md"), "# Disposable prototype\n").map_err(display_error)?;
    git(&project, &["add", "README.md"])?;
    git(&project, &["commit", "-qm", "test: seed prototype"])?;
    fs::canonicalize(project).map_err(display_error)
}

fn write_runtime_fixture(
    _repository: &Path,
    data_dir: &Path,
    docker: &Path,
    codex: &Path,
) -> Result<PathBuf, String> {
    let runtime_dir = data_dir.join("ephemeral-runtime");
    fs::create_dir(&runtime_dir).map_err(display_error)?;
    let config = runtime_dir.join("codex-worker.json");
    let value = json!({
        "docker_binary": docker,
        "docker_sha256": sha256_file(docker)?,
        "docker_version": "Docker version 28.0.4, build fixture",
        "docker_host": "unix:///fixture/docker.sock",
        "worker_image": format!("registry.invalid/tyrion-worker@sha256:{}", "b".repeat(64)),
        "worker_image_id": format!("sha256:{}", "1".repeat(64)),
        "codex_binary": codex,
        "codex_version": EXPECTED_CODEX,
        "codex_sha256": sha256_file(codex)?,
        "model": "fixture-model",
        "lease_ttl_seconds": 30,
        "vcpus": 2,
        "memory_mib": 6144,
        "writable_storage_mib": 4096,
        "max_processes": 256
    });
    fs::write(
        &config,
        serde_json::to_vec_pretty(&value).map_err(display_error)?,
    )
    .map_err(display_error)?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).map_err(display_error)?;
    Ok(config)
}

fn mcp_request(
    input: &mut impl Write,
    output: &mut impl BufRead,
    request: Value,
) -> Result<Value, String> {
    writeln!(input, "{request}").map_err(display_error)?;
    input.flush().map_err(display_error)?;
    let mut line = String::new();
    output.read_line(&mut line).map_err(display_error)?;
    if line.is_empty() {
        return Err("Entry MCP closed without a response".into());
    }
    serde_json::from_str(&line).map_err(display_error)
}

fn ensure_tool_success(response: &Value) -> Result<(), String> {
    if response.pointer("/result/isError").and_then(Value::as_bool) == Some(false) {
        Ok(())
    } else {
        Err(format!("Tyrion Entry tool failed: {response}"))
    }
}

fn required_text(value: &Value, pointer: &str) -> Result<String, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("response omitted {pointer}: {value}"))
}

fn write_executable(path: &Path, contents: &str) -> Result<PathBuf, String> {
    fs::write(path, contents).map_err(display_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(display_error)?;
    Ok(path.to_owned())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(fs::read(path).map_err(display_error)?)
    ))
}

fn git(path: &Path, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .map_err(display_error)?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn git_text(path: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .map_err(display_error)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(display_error)? {
            return status
                .success()
                .then_some(())
                .ok_or_else(|| format!("Entry MCP exited with {status}"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Entry MCP did not stop after input closed".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn render(state: &PrototypeState) {
    if io::stdout().is_terminal() {
        print!("\x1b[2J\x1b[H");
    }
    println!("{}", state.render());
}

fn invoked_as_codex() -> bool {
    env::args_os().next().is_some_and(|argument| {
        Path::new(&argument)
            .file_name()
            .is_some_and(|name| name == "codex")
    })
}

fn required_env(name: &str) -> Result<OsString, String> {
    env::var_os(name).ok_or_else(|| format!("{name} was not provided"))
}

fn require_regular_file(path: &Path, label: &str) -> Result<(), String> {
    path.is_file()
        .then_some(())
        .ok_or_else(|| format!("{label} is missing at {}", path.display()))
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

struct ScratchRoot {
    path: PathBuf,
}

struct ChildProcessGroup {
    id: i32,
}

impl ChildProcessGroup {
    fn new(id: u32) -> Self {
        Self {
            id: i32::try_from(id).expect("child process id fits i32"),
        }
    }

    fn terminate(&self) {
        if self.id > 0 {
            // SAFETY: the child was placed in a new process group whose ID is
            // its validated positive PID. No unrelated process joins it.
            unsafe {
                libc::kill(-self.id, libc::SIGTERM);
            }
        }
    }
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl ScratchRoot {
    fn new(parent: &Path) -> Result<Self, String> {
        let id = Uuid::new_v4().simple().to_string();
        let path = parent.join(format!("PROTOTYPE-zc-{}", &id[..8]));
        fs::create_dir(&path).map_err(display_error)?;
        Ok(Self { path })
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let safe_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("PROTOTYPE-zc-"));
        if safe_name {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

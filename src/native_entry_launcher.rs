use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::entry_mcp::NATIVE_ENTRY_INSTRUCTIONS;
use crate::protocol::{Command as TyrionCommand, Request, PROTOCOL_VERSION};
use crate::{send_request, NativeHarness, TyrionError};

pub fn launch_native_entry(
    harness: NativeHarness,
    explicit_socket: Option<&Path>,
    harness_arguments: &[String],
) -> Result<(), TyrionError> {
    let project_root = current_git_root(harness)?;
    let daemon = ensure_local_daemon(explicit_socket)?;
    let arguments = native_harness_arguments(harness, &daemon.socket, harness_arguments)?;
    let status = Command::new(harness.as_str())
        .args(arguments)
        .current_dir(&project_root)
        .env("TYRION_SOCKET", &daemon.socket)
        .env("TYRION_PROJECT_ROOT", &project_root)
        .status()
        .map_err(|error| {
            TyrionError::InvalidRequest(format!(
                "could not launch installed {} from PATH: {error}",
                harness.as_str()
            ))
        })?;
    if !status.success() {
        return Err(TyrionError::AttachmentRejected(format!(
            "{} Entry Session exited with {status}",
            harness.as_str()
        )));
    }
    Ok(())
}

struct LocalDaemon {
    socket: PathBuf,
    child: Option<Child>,
    _session_lock: Option<File>,
}

impl Drop for LocalDaemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn native_harness_arguments(
    harness: NativeHarness,
    socket: &Path,
    forwarded: &[String],
) -> Result<Vec<String>, TyrionError> {
    let executable = std::env::current_exe()?;
    let executable = executable.to_str().ok_or_else(|| {
        TyrionError::InvalidRequest("Tyrion executable path must be UTF-8".into())
    })?;
    let socket = socket
        .to_str()
        .ok_or_else(|| TyrionError::InvalidRequest("Tyrion socket path must be UTF-8".into()))?;
    let mcp_arguments = [
        "--socket",
        socket,
        "entry-mcp",
        "--harness",
        harness.as_str(),
    ];
    let mut arguments = match harness {
        NativeHarness::Claude => {
            let config = serde_json::to_string(&serde_json::json!({
                "mcpServers": {
                    "tyrion": {
                        "type": "stdio",
                        "command": executable,
                        "args": mcp_arguments,
                    }
                }
            }))?;
            vec![
                "--mcp-config".into(),
                config,
                "--append-system-prompt".into(),
                NATIVE_ENTRY_INSTRUCTIONS.into(),
            ]
        }
        NativeHarness::Codex => vec![
            "-c".into(),
            format!(
                "mcp_servers.tyrion.command={}",
                serde_json::to_string(executable)?
            ),
            "-c".into(),
            format!(
                "mcp_servers.tyrion.args={}",
                serde_json::to_string(&mcp_arguments)?
            ),
            "-c".into(),
            "mcp_servers.tyrion.required=true".into(),
            "-c".into(),
            "mcp_servers.tyrion.default_tools_approval_mode=\"auto\"".into(),
            "-c".into(),
            format!(
                "developer_instructions={}",
                serde_json::to_string(NATIVE_ENTRY_INSTRUCTIONS)?
            ),
        ],
    };
    arguments.extend_from_slice(forwarded);
    Ok(arguments)
}

fn current_git_root(harness: NativeHarness) -> Result<PathBuf, TyrionError> {
    let current = std::env::current_dir()?;
    current
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(PathBuf::from)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(format!(
                "tyrion {} must be launched inside a Git repository",
                harness.as_str()
            ))
        })
}

fn ensure_local_daemon(explicit_socket: Option<&Path>) -> Result<LocalDaemon, TyrionError> {
    if let Some(socket) = explicit_socket {
        if !daemon_is_ready(socket) {
            return Err(TyrionError::InvalidRequest(format!(
                "no Tyrion daemon is listening at {}",
                socket.display()
            )));
        }
        return Ok(LocalDaemon {
            socket: socket.to_path_buf(),
            child: None,
            _session_lock: None,
        });
    }

    let data_dir = default_data_dir()?;
    fs::create_dir_all(&data_dir)?;
    let metadata = fs::symlink_metadata(&data_dir)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(TyrionError::InvalidRequest(
            "Tyrion data directory must be a user-owned regular directory".into(),
        ));
    }
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))?;
    let session_lock = open_native_session_lock(&data_dir)?;
    let socket = data_dir.join("tyrion.sock");
    if daemon_is_ready(&socket) {
        return Ok(LocalDaemon {
            socket,
            child: None,
            _session_lock: Some(session_lock),
        });
    }

    let daemon_binary = std::env::current_exe()?.with_file_name("tyriond");
    let mut command = Command::new(&daemon_binary);
    command
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--socket")
        .arg(&socket);
    // Written by `tyrion init`. Without it the daemon runs only the
    // deterministic Worker, which cannot touch a repository.
    let runtime = data_dir.join("runtime");
    if runtime.join(RUNTIME_CONFIG).is_file() {
        command
            .arg("--codex-worker-config")
            .arg(runtime.join(RUNTIME_CONFIG));
        if runtime.join(RUNTIME_CATALOG).is_file() {
            command
                .arg("--worker-catalog")
                .arg(runtime.join(RUNTIME_CATALOG));
        }
    }
    let log = data_dir.join("tyriond.log");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(&log)?))
        .spawn()
        .map_err(|error| {
            TyrionError::InvalidRequest(format!(
                "failed to start {}: {error}",
                daemon_binary.display()
            ))
        })?;
    // Startup verifies every pinned binary and the Docker engine, which takes
    // longer than binding a socket.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if daemon_is_ready(&socket) {
            return Ok(LocalDaemon {
                socket,
                child: Some(child),
                _session_lock: Some(session_lock),
            });
        }
        if let Some(status) = child.try_wait()? {
            let reason = fs::read_to_string(&log).unwrap_or_default();
            return Err(TyrionError::InvalidRequest(format!(
                "Tyrion daemon exited before becoming ready ({status}): {}\n  next: rerun `tyrion init`",
                reason.trim()
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TyrionError::InvalidRequest(
                "Tyrion daemon did not become ready within 30 seconds".into(),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn open_native_session_lock(data_dir: &Path) -> Result<File, TyrionError> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(data_dir.join("native-entry.lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TyrionError::InvalidRequest(
            "native Entry lock must be a user-owned regular file".into(),
        ));
    }
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(TyrionError::InvalidRequest(
                "another auto-managed native Entry Session is already running; reuse that TUI or connect both sessions to an explicitly managed --socket"
                    .into(),
            ))
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn daemon_is_ready(socket: &Path) -> bool {
    send_request(
        socket,
        &Request {
            protocol_version: PROTOCOL_VERSION,
            attachment_token: None,
            principal_token: None,
            idempotency_key: None,
            expected_revision: None,
            expected_control_revision: None,
            command: TyrionCommand::InspectCommission {
                commission_id: "native-entry-readiness-probe".into(),
            },
        },
    )
    .is_ok()
}

/// The runtime `tyrion init` generates, inside the data directory's `runtime/`.
pub(crate) const RUNTIME_CONFIG: &str = "worker-runtime.json";
pub(crate) const RUNTIME_CATALOG: &str = "worker-catalog.json";

pub(crate) fn default_data_dir() -> Result<PathBuf, TyrionError> {
    if let Some(path) = std::env::var_os("TYRION_DATA_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("tyrion"));
    }
    if let Some(path) = std::env::var_os("HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join(".local/state/tyrion"));
    }
    Err(TyrionError::InvalidRequest(
        "set TYRION_DATA_DIR or HOME so Tyrion can store durable local state".into(),
    ))
}

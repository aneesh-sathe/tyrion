use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use fs2::FileExt;
use serde_json::Value;

use crate::protocol::{Command, Request, Response, PROTOCOL_VERSION};
use crate::store::Store;
use crate::TyrionError;

const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

pub fn run_daemon(data_dir: &Path, socket_path: &Path) -> Result<(), TyrionError> {
    run_daemon_with_options(data_dir, socket_path, DaemonOptions::default())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DaemonOptions {
    pub defer_ready_dispatch: bool,
}

pub fn run_daemon_with_options(
    data_dir: &Path,
    socket_path: &Path,
    options: DaemonOptions,
) -> Result<(), TyrionError> {
    fs::create_dir_all(data_dir)?;
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))?;
    let _ownership = acquire_ownership(data_dir)?;
    prepare_socket(socket_path)?;
    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    let mut store = Store::open(&data_dir.join("state.sqlite3"))?;
    if !options.defer_ready_dispatch {
        resume_ready_assignments(&mut store)?;
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => serve_connection(stream, &mut store, options),
            Err(error) => eprintln!("failed to accept local connection: {error}"),
        }
    }
    Ok(())
}

fn resume_ready_assignments(store: &mut Store) -> Result<(), TyrionError> {
    for commission_id in store.ready_commission_ids()? {
        match store.run_ready_assignment(&commission_id) {
            Ok(()) => {}
            Err(TyrionError::InvalidRequest(message)) => {
                eprintln!(
                    "could not resume ready Assignment for Commission {commission_id}: {message}"
                );
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn acquire_ownership(data_dir: &Path) -> Result<File, TyrionError> {
    let lock_path = data_dir.join("control-plane.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    fs::set_permissions(lock_path, fs::Permissions::from_mode(0o600))?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(lock),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(TyrionError::InvalidRequest(format!(
                "data directory {} is already owned by another Control Plane",
                data_dir.display()
            )))
        }
        Err(error) => Err(TyrionError::Io(error)),
    }
}

fn prepare_socket(socket_path: &Path) -> Result<(), TyrionError> {
    let Some(parent) = socket_path.parent() else {
        return Err(TyrionError::InvalidRequest(
            "socket path must have a parent directory".into(),
        ));
    };
    fs::create_dir_all(parent)?;
    if socket_path.exists() {
        let metadata = fs::symlink_metadata(socket_path)?;
        if !metadata.file_type().is_socket() {
            return Err(TyrionError::InvalidRequest(format!(
                "refusing to replace non-socket path {}",
                socket_path.display()
            )));
        }
        fs::remove_file(socket_path)?;
    }
    Ok(())
}

fn serve_connection(mut stream: UnixStream, store: &mut Store, options: DaemonOptions) {
    let outcome = read_request(&stream).and_then(|request| dispatch(store, &request));
    let (response, follow_up) = match outcome {
        Ok(outcome) => (Response::success(outcome.data), outcome.follow_up),
        Err(error) => (Response::failure(&error), None),
    };
    let write_result = (|| -> Result<(), TyrionError> {
        serde_json::to_writer(&mut stream, &response)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        stream.shutdown(Shutdown::Write)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        eprintln!("failed to write local response: {error}");
    }
    drop(stream);

    if !options.defer_ready_dispatch {
        let Some(FollowUp::RunReadyAssignment(commission_id)) = follow_up else {
            return;
        };
        if let Err(error) = store.run_ready_assignment(&commission_id) {
            eprintln!("failed to run ready Assignment for Commission {commission_id}: {error}");
        }
    }
}

fn read_request(stream: &UnixStream) -> Result<Request, TyrionError> {
    let mut request = Vec::new();
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES + 1);
    reader.read_until(b'\n', &mut request)?;
    if request.len() as u64 > MAX_REQUEST_BYTES {
        return Err(TyrionError::InvalidRequest(
            "request exceeds the 1 MiB protocol limit".into(),
        ));
    }
    if request.is_empty() {
        return Err(TyrionError::InvalidRequest("request is empty".into()));
    }
    serde_json::from_slice(&request).map_err(|error| {
        TyrionError::InvalidRequest(format!("request is not valid protocol JSON: {error}"))
    })
}

fn dispatch(store: &mut Store, request: &Request) -> Result<DispatchOutcome, TyrionError> {
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(TyrionError::UnsupportedVersion {
            actual: request.protocol_version,
            expected: PROTOCOL_VERSION,
        });
    }
    if request.command.is_mutating()
        && request
            .idempotency_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
    {
        return Err(TyrionError::InvalidRequest(
            "mutating requests require an idempotency key".into(),
        ));
    }

    match &request.command {
        Command::CreateProposal { proposal } => Ok(DispatchOutcome::complete(
            store.create_proposal(request, proposal)?,
        )),
        Command::InspectCommission { commission_id } => Ok(DispatchOutcome::complete(
            store.inspect_commission(commission_id)?,
        )),
        Command::AcceptCommission { commission_id } => Ok(DispatchOutcome {
            data: store.accept_commission(request, commission_id)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
    }
}

struct DispatchOutcome {
    data: Value,
    follow_up: Option<FollowUp>,
}

impl DispatchOutcome {
    fn complete(data: Value) -> Self {
        Self {
            data,
            follow_up: None,
        }
    }
}

enum FollowUp {
    RunReadyAssignment(String),
}

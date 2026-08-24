use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use fs2::FileExt;
use serde_json::Value;

use crate::protocol::{Command, Request, Response, PROTOCOL_VERSION};
use crate::store::Store;
use crate::worker::{WorkerRuntime, WorkerRuntimeOptions};
use crate::TyrionError;

const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

pub fn run_daemon(data_dir: &Path, socket_path: &Path) -> Result<(), TyrionError> {
    run_daemon_with_options(data_dir, socket_path, DaemonOptions::default())
}

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub defer_ready_dispatch: bool,
    pub corrupt_worker_artifact_revision: bool,
    pub incorrect_first_worker_result: bool,
    pub codex_worker_config: Option<PathBuf>,
    pub worker_catalog: Option<PathBuf>,
    pub hold_worker_for_control: bool,
    pub hold_worker_before_integration: bool,
    pub hold_worker_after_integration: bool,
    pub hold_worker_after_external_integration: bool,
    pub skip_sandbox_cleanup: bool,
    pub watchdog_stall_milliseconds: u64,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            defer_ready_dispatch: false,
            corrupt_worker_artifact_revision: false,
            incorrect_first_worker_result: false,
            codex_worker_config: None,
            worker_catalog: None,
            hold_worker_for_control: false,
            hold_worker_before_integration: false,
            hold_worker_after_integration: false,
            hold_worker_after_external_integration: false,
            skip_sandbox_cleanup: false,
            watchdog_stall_milliseconds: 30_000,
        }
    }
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
    let database_path = data_dir.join("state.sqlite3");
    let mut store = Store::open(&database_path)?;
    let worker = Arc::new(WorkerRuntime::load(
        data_dir,
        options.codex_worker_config.as_deref(),
        options.worker_catalog.as_deref(),
        WorkerRuntimeOptions {
            corrupt_artifact_revision: options.corrupt_worker_artifact_revision,
            incorrect_first_result: options.incorrect_first_worker_result,
            hold_for_control: options.hold_worker_for_control,
            hold_before_integration: options.hold_worker_before_integration,
            hold_after_integration: options.hold_worker_after_integration,
            hold_after_external_integration: options.hold_worker_after_external_integration,
        },
    )?);
    let pending_cleanups = store.recover_stranded_attempts()?;
    if !options.skip_sandbox_cleanup {
        for cleanup in pending_cleanups {
            match worker.cleanup_stranded_attempt(
                &cleanup.attempt_id,
                &cleanup.commission_id,
                &cleanup.execution,
                cleanup.artifact_revision.as_deref(),
            ) {
                Ok(()) => store.complete_sandbox_cleanup(&cleanup.attempt_id)?,
                Err(error) => {
                    eprintln!(
                        "containment cleanup for Attempt {} remains pending: {error}",
                        cleanup.attempt_id
                    )
                }
            }
        }
    }
    if !options.defer_ready_dispatch {
        resume_ready_assignments(&mut store, &database_path, &worker)?;
    }
    spawn_watchdog(
        database_path.clone(),
        Arc::clone(&worker),
        options.watchdog_stall_milliseconds,
    )?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => serve_connection(stream, &mut store, &options, &database_path, &worker),
            Err(error) => eprintln!("failed to accept local connection: {error}"),
        }
    }
    Ok(())
}

fn spawn_watchdog(
    database_path: PathBuf,
    worker: Arc<WorkerRuntime>,
    stall_milliseconds: u64,
) -> Result<(), TyrionError> {
    thread::Builder::new()
        .name("commission-watchdog".into())
        .spawn(move || {
            let mut store = match Store::open(&database_path) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("Commission Watchdog could not open durable state: {error}");
                    return;
                }
            };
            loop {
                match store.watchdog_sweep(&worker, stall_milliseconds) {
                    Ok(commission_ids) => {
                        for commission_id in commission_ids {
                            if let Err(error) = spawn_ready_assignment(
                                database_path.clone(),
                                Arc::clone(&worker),
                                commission_id.clone(),
                            ) {
                                eprintln!(
                                    "Commission Watchdog could not redispatch {commission_id}: {error}"
                                );
                            }
                        }
                    }
                    Err(error) => eprintln!("Commission Watchdog sweep failed: {error}"),
                }
                thread::sleep(std::time::Duration::from_millis(25));
            }
        })?;
    Ok(())
}

fn resume_ready_assignments(
    store: &mut Store,
    database_path: &Path,
    worker: &Arc<WorkerRuntime>,
) -> Result<(), TyrionError> {
    for commission_id in store.ready_commission_ids()? {
        spawn_ready_assignment(database_path.to_owned(), Arc::clone(worker), commission_id)?;
    }
    Ok(())
}

fn spawn_ready_assignment(
    database_path: PathBuf,
    worker: Arc<WorkerRuntime>,
    commission_id: String,
) -> Result<(), TyrionError> {
    let thread_label = commission_id.chars().take(12).collect::<String>();
    thread::Builder::new()
        .name(format!("assignment-{thread_label}"))
        .spawn(move || {
            let outcome = (|| {
                loop {
                    let store = Store::open(&database_path)?;
                    let (parallelism, ready_before, attempts_before) =
                        store.dispatch_snapshot(&commission_id)?;
                    drop(store);
                    if ready_before == 0 {
                        break;
                    }
                    let round = (0..parallelism)
                        .map(|index| {
                            let database_path = database_path.clone();
                            let worker = Arc::clone(&worker);
                            let commission_id = commission_id.clone();
                            thread::Builder::new()
                                .name(format!("worker-{thread_label}-{index}"))
                                .spawn(move || {
                                    let mut store = Store::open(&database_path)?;
                                    store.run_ready_assignment(&commission_id, &worker)
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    for worker_thread in round {
                        worker_thread.join().map_err(|_| {
                            TyrionError::InvalidRequest(
                                "Assignment dispatch thread terminated unexpectedly".into(),
                            )
                        })??;
                    }
                    let store = Store::open(&database_path)?;
                    let (_, ready_after, attempts_after) =
                        store.dispatch_snapshot(&commission_id)?;
                    if ready_after == 0
                        || (ready_after == ready_before && attempts_after == attempts_before)
                    {
                        break;
                    }
                }
                Ok::<(), TyrionError>(())
            })();
            if let Err(error) = outcome {
                eprintln!("failed to run ready Assignment for Commission {commission_id}: {error}");
            }
        })?;
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

fn serve_connection(
    mut stream: UnixStream,
    store: &mut Store,
    options: &DaemonOptions,
    database_path: &Path,
    worker: &Arc<WorkerRuntime>,
) {
    let outcome = read_request(&stream).and_then(|request| dispatch(store, worker, &request));
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
        if let Err(error) =
            spawn_ready_assignment(database_path.to_owned(), Arc::clone(worker), commission_id)
        {
            eprintln!("failed to schedule ready Assignment: {error}");
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

fn dispatch(
    store: &mut Store,
    worker: &WorkerRuntime,
    request: &Request,
) -> Result<DispatchOutcome, TyrionError> {
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
        Command::CreateProposal { proposal } => Ok(DispatchOutcome::without_follow_up(
            store.create_proposal(request, proposal)?,
        )),
        Command::InspectCommission { commission_id } => Ok(DispatchOutcome::without_follow_up(
            store.inspect_commission(request, commission_id, worker)?,
        )),
        Command::AcceptCommission { commission_id } => Ok(DispatchOutcome {
            data: store.accept_commission(request, commission_id, worker)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
        Command::PauseCommission { commission_id } => Ok(DispatchOutcome::without_follow_up(
            store.pause_commission(request, commission_id)?,
        )),
        Command::ResumeCommission { commission_id } => Ok(DispatchOutcome {
            data: store.resume_commission(request, commission_id)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
        Command::CancelCommission { commission_id } => Ok(DispatchOutcome::without_follow_up(
            store.cancel_commission(request, commission_id, worker)?,
        )),
        Command::RecordVerificationEvidence {
            commission_id,
            evidence,
        } => Ok(DispatchOutcome {
            data: store.record_verification_evidence(request, commission_id, evidence)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
        Command::AmendVerification {
            commission_id,
            amendment,
        } => Ok(DispatchOutcome {
            data: store.amend_verification(request, commission_id, amendment)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
        Command::IssueAttachmentToken {
            expected_adapter,
            ttl_seconds,
        } => Ok(DispatchOutcome::without_follow_up(
            store.issue_attachment_token(request, expected_adapter, *ttl_seconds)?,
        )),
        Command::ConnectAttachment {
            launch_token,
            handshake,
            replay,
        } => Ok(DispatchOutcome::without_follow_up(
            store.connect_attachment(request, launch_token, handshake, replay.as_ref())?,
        )),
        Command::ResumeAttachment { handshake, replay } => Ok(DispatchOutcome::without_follow_up(
            store.resume_attachment(request, handshake, replay)?,
        )),
        Command::TakeControl { commission_id } => Ok(DispatchOutcome::without_follow_up(
            store.take_control(request, commission_id)?,
        )),
        Command::ReplayEvents {
            commission_id,
            after_sequence,
        } => Ok(DispatchOutcome::without_follow_up(store.replay_events(
            request,
            commission_id,
            *after_sequence,
        )?)),
        Command::SteerWorker {
            commission_id,
            worker_handle,
            clarification,
        } => Ok(DispatchOutcome::without_follow_up(store.steer_worker(
            request,
            commission_id,
            worker_handle,
            clarification,
            worker,
        )?)),
        Command::InterruptWorker {
            commission_id,
            worker_handle,
            reason,
        } => Ok(DispatchOutcome::without_follow_up(store.interrupt_worker(
            request,
            commission_id,
            worker_handle,
            reason,
            worker,
        )?)),
        Command::RetryWorker {
            commission_id,
            worker_handle,
        } => Ok(DispatchOutcome {
            data: store.retry_worker(request, commission_id, worker_handle)?,
            follow_up: Some(FollowUp::RunReadyAssignment(commission_id.clone())),
        }),
    }
}

struct DispatchOutcome {
    data: Value,
    follow_up: Option<FollowUp>,
}

impl DispatchOutcome {
    fn without_follow_up(data: Value) -> Self {
        Self {
            data,
            follow_up: None,
        }
    }
}

enum FollowUp {
    RunReadyAssignment(String),
}

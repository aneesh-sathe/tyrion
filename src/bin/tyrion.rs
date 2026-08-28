use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use tyrion::protocol::{
    AdapterIdentity, AttachmentHandshake, Command, CommissionAmendment, CommissionProposal,
    CommissionReplayCursor, CredentialGrantRequest, LearningObservationKind,
    OperationReconciliationOutcome, OperationRequest, Request, ReusablePreference,
    VerificationAmendment, VerificationEvidenceSubmission, PROTOCOL_VERSION,
};

#[derive(Debug, Parser)]
#[command(about = "Review and control Tyrion Commissions")]
struct Arguments {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long, global = true)]
    attachment_token: Option<String>,
    #[arg(long, global = true)]
    principal_token_stdin: bool,
    #[command(subcommand)]
    command: TopLevelCommand,
}

#[derive(Debug, Subcommand)]
enum TopLevelCommand {
    /// Launch an explicitly attached Pi Entry Session.
    Pi {
        #[arg(long, default_value = "pi")]
        pi_command: PathBuf,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long)]
        commission_id: Option<String>,
        #[arg(long, default_value_t = 0)]
        last_event_sequence: i64,
        #[arg(last = true)]
        pi_arguments: Vec<String>,
    },
    Proposal {
        #[command(subcommand)]
        command: ProposalCommand,
    },
    Commission {
        #[command(subcommand)]
        command: CommissionCommand,
    },
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
    },
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    Operation {
        #[command(subcommand)]
        command: OperationCommand,
    },
    Principal {
        #[command(subcommand)]
        command: PrincipalCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AttachmentCommand {
    IssueToken {
        #[arg(long)]
        harness: String,
        #[arg(long)]
        adapter_identity: String,
        #[arg(long)]
        adapter_version: String,
        #[arg(long, default_value_t = 60)]
        ttl_seconds: u64,
        #[arg(long)]
        idempotency_key: String,
    },
    Connect {
        #[arg(long)]
        token: String,
        #[arg(long)]
        harness: String,
        #[arg(long)]
        adapter_identity: String,
        #[arg(long)]
        adapter_version: String,
        #[arg(long, default_value_t = PROTOCOL_VERSION)]
        adapter_protocol_version: u16,
        #[arg(long)]
        native_session_id: String,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long)]
        commission_id: Option<String>,
        #[arg(long, default_value_t = 0)]
        last_event_sequence: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Resume {
        commission_id: String,
        #[arg(long)]
        harness: String,
        #[arg(long)]
        adapter_identity: String,
        #[arg(long)]
        adapter_version: String,
        #[arg(long, default_value_t = PROTOCOL_VERSION)]
        adapter_protocol_version: u16,
        #[arg(long)]
        native_session_id: String,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long, default_value_t = 0)]
        last_event_sequence: i64,
    },
    Replay {
        commission_id: String,
        #[arg(long, default_value_t = 0)]
        after_sequence: i64,
    },
}

#[derive(Debug, Subcommand)]
enum ProposalCommand {
    Create {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[derive(Debug, Subcommand)]
enum CommissionCommand {
    Inspect {
        commission_id: String,
    },
    Accept {
        commission_id: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Pause {
        commission_id: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Resume {
        commission_id: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Cancel {
        commission_id: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    RecordEvidence {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    AmendVerification {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    ProposeAmendment {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    TakeControl {
        commission_id: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        expected_control_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[derive(Debug, Subcommand)]
enum WorkerCommand {
    Steer {
        commission_id: String,
        worker_handle: String,
        #[arg(long)]
        clarification: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Interrupt {
        commission_id: String,
        worker_handle: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Retry {
        commission_id: String,
        worker_handle: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[derive(Debug, Subcommand)]
enum OperationCommand {
    Propose {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    Execute {
        commission_id: String,
        approval_gate_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[derive(Debug, Subcommand)]
enum PrincipalCommand {
    RememberPreference {
        commission_id: String,
        #[arg(long)]
        statement: String,
        #[arg(long)]
        idempotency_key: String,
    },
    RevisePreference {
        commission_id: String,
        claim_id: String,
        #[arg(long)]
        statement: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        confirmation_digest: Option<String>,
        #[arg(long)]
        idempotency_key: String,
    },
    ObservePreference {
        commission_id: String,
        #[arg(long)]
        statement: String,
        #[arg(long)]
        outcome: LearningObservationKind,
        #[arg(long)]
        explanation: Option<String>,
        #[arg(long)]
        idempotency_key: String,
    },
    ConfirmPreference {
        commission_id: String,
        claim_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    SuppressPreference {
        commission_id: String,
        claim_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    ForgetPreference {
        commission_id: String,
        claim_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        confirmation_digest: Option<String>,
        #[arg(long)]
        idempotency_key: String,
    },
    PreventPreference {
        commission_id: String,
        #[arg(long)]
        statement: String,
        #[arg(long)]
        idempotency_key: String,
    },
    ExportMemory {
        #[arg(long)]
        project_id: Option<String>,
    },
    ImportMemory {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        idempotency_key: String,
    },
    PinMemoryMaterial {
        commission_id: String,
        material_id: String,
        #[arg(long)]
        idempotency_key: String,
    },
    InspectClaim {
        claim_id: String,
    },
    InspectProfile {
        #[arg(long)]
        project_id: Option<String>,
    },
    GrantCredential {
        commission_id: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    InspectGate {
        approval_gate_id: String,
    },
    ApproveGate {
        commission_id: String,
        approval_gate_id: String,
        #[arg(long)]
        expected_operation_digest: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    ReconcileOperation {
        commission_id: String,
        operation_request_id: String,
        #[arg(long)]
        outcome: OperationReconciliationOutcome,
        #[arg(long)]
        observed_sha256: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
    InspectAmendment {
        amendment_id: String,
    },
    AcceptAmendment {
        commission_id: String,
        amendment_id: String,
        #[arg(long)]
        expected_amendment_digest: String,
        #[arg(long)]
        expected_revision: i64,
        #[arg(long)]
        idempotency_key: String,
    },
}

fn main() {
    let arguments = Arguments::parse();
    if let TopLevelCommand::Pi {
        pi_command,
        capabilities,
        commission_id,
        last_event_sequence,
        pi_arguments,
    } = &arguments.command
    {
        if let Err(error) = launch_pi(
            &arguments.socket,
            pi_command,
            capabilities,
            commission_id.as_deref(),
            *last_event_sequence,
            pi_arguments,
        ) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let request = match build_request(&arguments) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    match tyrion::send_request(&arguments.socket, &request) {
        Ok(response) if response.ok => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response.data.expect("successful response has data"))
                    .expect("response data should serialize")
            );
        }
        Ok(response) => {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&response.error.expect("failed response has error"))
                    .expect("response error should serialize")
            );
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

const PI_ADAPTER_IDENTITY: &str = "tyrion-pi-entry";
const PI_ADAPTER_VERSION: &str = "1.0.0";
const PI_EXTENSION: &str = include_str!("../../adapters/pi_entry_extension.mjs");
const FULL_ENTRY_CAPABILITIES: [&str; 9] = [
    "proposal_creation",
    "commission_acceptance",
    "commission_inspection",
    "event_replay",
    "control_takeover",
    "material_notifications",
    "persistent_mode_display",
    "worker_steering",
    "worker_interruption",
];

fn launch_pi(
    socket: &std::path::Path,
    pi_command: &std::path::Path,
    capabilities: &[String],
    commission_id: Option<&str>,
    last_event_sequence: i64,
    pi_arguments: &[String],
) -> Result<(), tyrion::TyrionError> {
    if last_event_sequence < 0 {
        return Err(tyrion::TyrionError::InvalidRequest(
            "last durable event cursor must not be negative".into(),
        ));
    }
    if pi_arguments.iter().any(|argument| {
        matches!(argument.as_str(), "--extension" | "-e") || argument.starts_with("--extension=")
    }) {
        return Err(tyrion::TyrionError::InvalidRequest(
            "Pi Entry launch forbids additional extension arguments".into(),
        ));
    }
    let capabilities = if capabilities.is_empty() {
        FULL_ENTRY_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_owned())
            .collect::<Vec<_>>()
    } else {
        capabilities.to_vec()
    };
    let issue_request = Request {
        protocol_version: PROTOCOL_VERSION,
        attachment_token: None,
        principal_token: None,
        idempotency_key: Some(format!("pi-launch-{}", uuid::Uuid::new_v4())),
        expected_revision: None,
        expected_control_revision: None,
        command: Command::IssueAttachmentToken {
            expected_adapter: AdapterIdentity {
                harness: "pi".into(),
                adapter_identity: PI_ADAPTER_IDENTITY.into(),
                adapter_version: PI_ADAPTER_VERSION.into(),
            },
            ttl_seconds: 60,
        },
    };
    let response = tyrion::send_request(socket, &issue_request)?;
    if !response.ok {
        return Err(tyrion::TyrionError::AttachmentRejected(
            response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "launch token request failed".into()),
        ));
    }
    let launch_token = response
        .data
        .as_ref()
        .and_then(|data| data["launch_token"].as_str())
        .ok_or_else(|| {
            tyrion::TyrionError::AttachmentRejected("launch token response was incomplete".into())
        })?;
    let extension_path = materialize_pi_extension(socket)?;
    let extension_path = extension_path.to_str().ok_or_else(|| {
        tyrion::TyrionError::InvalidRequest("Pi Entry extension path must be UTF-8".into())
    })?;
    let mut command = ProcessCommand::new(pi_command);
    command
        .args(["--no-extensions", "--extension", extension_path])
        .args(pi_arguments)
        .env("TYRION_PI_SOCKET", socket)
        .env("TYRION_PI_LAUNCH_TOKEN", launch_token)
        .env(
            "TYRION_PI_CAPABILITIES",
            serde_json::to_string(&capabilities)?,
        )
        .env(
            "TYRION_PI_LAST_EVENT_SEQUENCE",
            last_event_sequence.to_string(),
        );
    if let Some(commission_id) = commission_id {
        command.env("TYRION_PI_COMMISSION_ID", commission_id);
    } else {
        command.env_remove("TYRION_PI_COMMISSION_ID");
    }
    let status = command.status()?;
    if !status.success() {
        return Err(tyrion::TyrionError::AttachmentRejected(format!(
            "Pi Entry Session exited with {status}"
        )));
    }
    Ok(())
}

fn materialize_pi_extension(socket: &std::path::Path) -> Result<PathBuf, tyrion::TyrionError> {
    let parent = socket.parent().ok_or_else(|| {
        tyrion::TyrionError::InvalidRequest("Tyrion socket requires a parent directory".into())
    })?;
    validate_pi_cache_parent(parent)?;
    let directory = parent.join("entry-adapters");
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let directory_metadata = fs::symlink_metadata(&directory)?;
    let current_user = unsafe { libc::geteuid() };
    if !directory_metadata.is_dir()
        || directory_metadata.file_type().is_symlink()
        || directory_metadata.uid() != current_user
    {
        return Err(tyrion::TyrionError::AttachmentRejected(
            "Pi Entry adapter cache must be a user-owned regular directory".into(),
        ));
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let directory_handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&directory)?;
    let secured_metadata = directory_handle.metadata()?;
    if !secured_metadata.is_dir()
        || secured_metadata.uid() != current_user
        || secured_metadata.permissions().mode() & 0o077 != 0
    {
        return Err(tyrion::TyrionError::AttachmentRejected(
            "Pi Entry adapter cache permissions are not private".into(),
        ));
    }
    let digest = format!("{:x}", Sha256::digest(PI_EXTENSION.as_bytes()));
    let file_name = format!("pi-entry-{digest}.mjs");
    let native_name = CString::new(file_name.as_bytes()).map_err(|_| {
        tyrion::TyrionError::InvalidRequest("Pi Entry extension name is invalid".into())
    })?;
    let create_flags =
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let created = unsafe {
        libc::openat(
            directory_handle.as_raw_fd(),
            native_name.as_ptr(),
            create_flags,
            0o600,
        )
    };
    let (mut file, created_new) = if created >= 0 {
        (unsafe { File::from_raw_fd(created) }, true)
    } else {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
        let existing = unsafe {
            libc::openat(
                directory_handle.as_raw_fd(),
                native_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if existing < 0 {
            return Err(tyrion::TyrionError::AttachmentRejected(
                "cached Pi Entry extension is not a regular file".into(),
            ));
        }
        (unsafe { File::from_raw_fd(existing) }, false)
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_user {
        return Err(tyrion::TyrionError::AttachmentRejected(
            "cached Pi Entry extension must be a user-owned regular file".into(),
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    if created_new {
        use std::io::Write;
        file.write_all(PI_EXTENSION.as_bytes())?;
        file.sync_all()?;
        directory_handle.sync_all()?;
    } else {
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        let actual = format!("{:x}", Sha256::digest(contents));
        if actual != digest {
            return Err(tyrion::TyrionError::AttachmentRejected(
                "cached Pi Entry extension failed its pinned digest".into(),
            ));
        }
    }
    Ok(directory.join(file_name))
}

fn validate_pi_cache_parent(parent: &std::path::Path) -> Result<(), tyrion::TyrionError> {
    let current_user = unsafe { libc::geteuid() };
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != current_user
    {
        return Err(tyrion::TyrionError::AttachmentRejected(
            "Tyrion socket parent must be a user-owned regular directory".into(),
        ));
    }
    let absolute_parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()?.join(parent)
    };
    validate_pi_cache_ancestor_chain(&absolute_parent)?;
    validate_pi_cache_ancestor_chain(&fs::canonicalize(parent)?)?;
    Ok(())
}

fn validate_pi_cache_ancestor_chain(
    directory: &std::path::Path,
) -> Result<(), tyrion::TyrionError> {
    for ancestor in directory.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
            return Err(tyrion::TyrionError::AttachmentRejected(
                "Pi Entry adapter cache requires a socket parent chain that other users cannot modify"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn attachment_handshake(
    harness: &str,
    adapter_identity: &str,
    adapter_version: &str,
    adapter_protocol_version: u16,
    native_session_id: &str,
    capabilities: &[String],
) -> Box<AttachmentHandshake> {
    Box::new(AttachmentHandshake {
        adapter: AdapterIdentity {
            harness: harness.to_owned(),
            adapter_identity: adapter_identity.to_owned(),
            adapter_version: adapter_version.to_owned(),
        },
        adapter_protocol_version,
        native_session_id: native_session_id.to_owned(),
        capabilities: capabilities.to_owned(),
    })
}

fn build_request(arguments: &Arguments) -> Result<Request, tyrion::TyrionError> {
    let (command, idempotency_key, expected_revision, expected_control_revision) =
        match &arguments.command {
            TopLevelCommand::Pi { .. } => {
                return Err(tyrion::TyrionError::InvalidRequest(
                    "Pi launch must be handled before protocol request construction".into(),
                ));
            }
            TopLevelCommand::Proposal {
                command:
                    ProposalCommand::Create {
                        file,
                        idempotency_key,
                    },
            } => {
                let proposal: CommissionProposal = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::CreateProposal {
                        proposal: Box::new(proposal),
                    },
                    Some(idempotency_key.clone()),
                    None,
                    None,
                )
            }
            TopLevelCommand::Commission {
                command: CommissionCommand::Inspect { commission_id },
            } => (
                Command::InspectCommission {
                    commission_id: commission_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::Accept {
                        commission_id,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::AcceptCommission {
                    commission_id: commission_id.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::Pause {
                        commission_id,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::PauseCommission {
                    commission_id: commission_id.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::Resume {
                        commission_id,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::ResumeCommission {
                    commission_id: commission_id.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::Cancel {
                        commission_id,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::CancelCommission {
                    commission_id: commission_id.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::RecordEvidence {
                        commission_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let evidence: VerificationEvidenceSubmission =
                    serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::RecordVerificationEvidence {
                        commission_id: commission_id.clone(),
                        evidence: Box::new(evidence),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::ProposeAmendment {
                        commission_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let amendment: CommissionAmendment = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::ProposeCommissionAmendment {
                        commission_id: commission_id.clone(),
                        amendment: Box::new(amendment),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::AmendVerification {
                        commission_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let amendment: VerificationAmendment = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::AmendVerification {
                        commission_id: commission_id.clone(),
                        amendment: Box::new(amendment),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Commission {
                command:
                    CommissionCommand::TakeControl {
                        commission_id,
                        expected_revision,
                        expected_control_revision,
                        idempotency_key,
                    },
            } => (
                Command::TakeControl {
                    commission_id: commission_id.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                Some(*expected_control_revision),
            ),
            TopLevelCommand::Attachment {
                command:
                    AttachmentCommand::IssueToken {
                        harness,
                        adapter_identity,
                        adapter_version,
                        ttl_seconds,
                        idempotency_key,
                    },
            } => (
                Command::IssueAttachmentToken {
                    expected_adapter: AdapterIdentity {
                        harness: harness.clone(),
                        adapter_identity: adapter_identity.clone(),
                        adapter_version: adapter_version.clone(),
                    },
                    ttl_seconds: *ttl_seconds,
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Attachment {
                command:
                    AttachmentCommand::Replay {
                        commission_id,
                        after_sequence,
                    },
            } => (
                Command::ReplayEvents {
                    commission_id: commission_id.clone(),
                    after_sequence: *after_sequence,
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Attachment {
                command:
                    AttachmentCommand::Connect {
                        token,
                        harness,
                        adapter_identity,
                        adapter_version,
                        adapter_protocol_version,
                        native_session_id,
                        capabilities,
                        commission_id,
                        last_event_sequence,
                        idempotency_key,
                    },
            } => (
                Command::ConnectAttachment {
                    launch_token: token.clone(),
                    handshake: attachment_handshake(
                        harness,
                        adapter_identity,
                        adapter_version,
                        *adapter_protocol_version,
                        native_session_id,
                        capabilities,
                    ),
                    replay: commission_id
                        .as_ref()
                        .map(|commission_id| CommissionReplayCursor {
                            commission_id: commission_id.clone(),
                            last_event_sequence: *last_event_sequence,
                        }),
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Attachment {
                command:
                    AttachmentCommand::Resume {
                        commission_id,
                        harness,
                        adapter_identity,
                        adapter_version,
                        adapter_protocol_version,
                        native_session_id,
                        capabilities,
                        last_event_sequence,
                    },
            } => (
                Command::ResumeAttachment {
                    handshake: attachment_handshake(
                        harness,
                        adapter_identity,
                        adapter_version,
                        *adapter_protocol_version,
                        native_session_id,
                        capabilities,
                    ),
                    replay: CommissionReplayCursor {
                        commission_id: commission_id.clone(),
                        last_event_sequence: *last_event_sequence,
                    },
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Worker {
                command:
                    WorkerCommand::Steer {
                        commission_id,
                        worker_handle,
                        clarification,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::SteerWorker {
                    commission_id: commission_id.clone(),
                    worker_handle: worker_handle.clone(),
                    clarification: clarification.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Worker {
                command:
                    WorkerCommand::Interrupt {
                        commission_id,
                        worker_handle,
                        reason,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::InterruptWorker {
                    commission_id: commission_id.clone(),
                    worker_handle: worker_handle.clone(),
                    reason: reason.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Worker {
                command:
                    WorkerCommand::Retry {
                        commission_id,
                        worker_handle,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::RetryWorker {
                    commission_id: commission_id.clone(),
                    worker_handle: worker_handle.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Operation {
                command:
                    OperationCommand::Propose {
                        commission_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let operation: OperationRequest = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::ProposeOperation {
                        commission_id: commission_id.clone(),
                        operation: Box::new(operation),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Operation {
                command:
                    OperationCommand::Execute {
                        commission_id,
                        approval_gate_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let operation: OperationRequest = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::ExecuteOperation {
                        commission_id: commission_id.clone(),
                        approval_gate_id: approval_gate_id.clone(),
                        operation: Box::new(operation),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::RememberPreference {
                        commission_id,
                        statement,
                        idempotency_key,
                    },
            } => (
                Command::CreateProfileClaim {
                    commission_id: commission_id.clone(),
                    preference: ReusablePreference {
                        statement: statement.clone(),
                    },
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::RevisePreference {
                        commission_id,
                        claim_id,
                        statement,
                        expected_version,
                        confirmation_digest,
                        idempotency_key,
                    },
            } => (
                Command::ReviseProfileClaim {
                    commission_id: commission_id.clone(),
                    claim_id: claim_id.clone(),
                    expected_version: *expected_version,
                    confirmation_digest: confirmation_digest.clone(),
                    preference: ReusablePreference {
                        statement: statement.clone(),
                    },
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ObservePreference {
                        commission_id,
                        statement,
                        outcome,
                        explanation,
                        idempotency_key,
                    },
            } => (
                Command::ObserveProfilePreference {
                    commission_id: commission_id.clone(),
                    preference: ReusablePreference {
                        statement: statement.clone(),
                    },
                    outcome: *outcome,
                    explanation: explanation.clone(),
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ConfirmPreference {
                        commission_id,
                        claim_id,
                        expected_version,
                        idempotency_key,
                    },
            } => (
                Command::ConfirmProfileClaim {
                    commission_id: commission_id.clone(),
                    claim_id: claim_id.clone(),
                    expected_version: *expected_version,
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::SuppressPreference {
                        commission_id,
                        claim_id,
                        expected_version,
                        idempotency_key,
                    },
            } => (
                Command::SuppressProfileClaim {
                    commission_id: commission_id.clone(),
                    claim_id: claim_id.clone(),
                    expected_version: *expected_version,
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ForgetPreference {
                        commission_id,
                        claim_id,
                        expected_version,
                        confirmation_digest,
                        idempotency_key,
                    },
            } => (
                Command::ForgetProfileClaim {
                    commission_id: commission_id.clone(),
                    claim_id: claim_id.clone(),
                    expected_version: *expected_version,
                    confirmation_digest: confirmation_digest.clone(),
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::PreventPreference {
                        commission_id,
                        statement,
                        idempotency_key,
                    },
            } => (
                Command::CreateLearningBoundary {
                    commission_id: commission_id.clone(),
                    preference: ReusablePreference {
                        statement: statement.clone(),
                    },
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command: PrincipalCommand::ExportMemory { project_id },
            } => (
                Command::ExportMemory {
                    project_id: project_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ImportMemory {
                        commission_id,
                        file,
                        idempotency_key,
                    },
            } => {
                let bundle: serde_json::Value = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::ImportMemory {
                        commission_id: commission_id.clone(),
                        bundle: Box::new(bundle),
                    },
                    Some(idempotency_key.clone()),
                    None,
                    None,
                )
            }
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::PinMemoryMaterial {
                        commission_id,
                        material_id,
                        idempotency_key,
                    },
            } => (
                Command::PinMemoryMaterial {
                    commission_id: commission_id.clone(),
                    material_id: material_id.clone(),
                },
                Some(idempotency_key.clone()),
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command: PrincipalCommand::InspectClaim { claim_id },
            } => (
                Command::InspectProfileClaim {
                    claim_id: claim_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command: PrincipalCommand::InspectProfile { project_id },
            } => (
                Command::InspectProfile {
                    project_id: project_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::GrantCredential {
                        commission_id,
                        file,
                        expected_revision,
                        idempotency_key,
                    },
            } => {
                let grant: CredentialGrantRequest = serde_json::from_slice(&fs::read(file)?)?;
                (
                    Command::GrantCredential {
                        commission_id: commission_id.clone(),
                        grant: Box::new(grant),
                    },
                    Some(idempotency_key.clone()),
                    Some(*expected_revision),
                    None,
                )
            }
            TopLevelCommand::Principal {
                command: PrincipalCommand::InspectGate { approval_gate_id },
            } => (
                Command::InspectApprovalGate {
                    approval_gate_id: approval_gate_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ApproveGate {
                        commission_id,
                        approval_gate_id,
                        expected_operation_digest,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::ApproveOperation {
                    commission_id: commission_id.clone(),
                    approval_gate_id: approval_gate_id.clone(),
                    expected_operation_digest: expected_operation_digest.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::ReconcileOperation {
                        commission_id,
                        operation_request_id,
                        outcome,
                        observed_sha256,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::ReconcileOperation {
                    commission_id: commission_id.clone(),
                    operation_request_id: operation_request_id.clone(),
                    outcome: *outcome,
                    observed_sha256: observed_sha256.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
            TopLevelCommand::Principal {
                command: PrincipalCommand::InspectAmendment { amendment_id },
            } => (
                Command::InspectCommissionAmendment {
                    amendment_id: amendment_id.clone(),
                },
                None,
                None,
                None,
            ),
            TopLevelCommand::Principal {
                command:
                    PrincipalCommand::AcceptAmendment {
                        commission_id,
                        amendment_id,
                        expected_amendment_digest,
                        expected_revision,
                        idempotency_key,
                    },
            } => (
                Command::AcceptCommissionAmendment {
                    commission_id: commission_id.clone(),
                    amendment_id: amendment_id.clone(),
                    expected_amendment_digest: expected_amendment_digest.clone(),
                },
                Some(idempotency_key.clone()),
                Some(*expected_revision),
                None,
            ),
        };
    let principal_token = if arguments.principal_token_stdin {
        let mut token = String::new();
        io::stdin().read_to_string(&mut token)?;
        Some(token.trim().to_owned())
    } else {
        None
    };
    Ok(Request {
        protocol_version: PROTOCOL_VERSION,
        attachment_token: arguments.attachment_token.clone(),
        principal_token,
        idempotency_key,
        expected_revision,
        expected_control_revision,
        command,
    })
}

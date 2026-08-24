use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tyrion::protocol::{
    AdapterIdentity, AttachmentHandshake, Command, CommissionAmendment, CommissionProposal,
    CommissionReplayCursor, CredentialGrantRequest, OperationReconciliationOutcome,
    OperationRequest, Request, VerificationAmendment, VerificationEvidenceSubmission,
    PROTOCOL_VERSION,
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

use std::fs;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tyrion::protocol::{Command, CommissionProposal, Request, PROTOCOL_VERSION};

#[derive(Debug, Parser)]
#[command(about = "Review and control Tyrion Commissions")]
struct Arguments {
    #[arg(long)]
    socket: PathBuf,
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
}

fn main() {
    let arguments = Arguments::parse();
    let request = match build_request(&arguments.command) {
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

fn build_request(command: &TopLevelCommand) -> Result<Request, tyrion::TyrionError> {
    let (command, idempotency_key, expected_revision) = match command {
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
        ),
    };
    Ok(Request {
        protocol_version: PROTOCOL_VERSION,
        idempotency_key,
        expected_revision,
        command,
    })
}

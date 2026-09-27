use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::protocol::{
    AdapterIdentity, AttachmentHandshake, Command, CommissionProposal, Request, VerifierType,
    PROTOCOL_VERSION,
};
use crate::{send_request, TyrionError};

const ADAPTER_IDENTITY: &str = "tyrion-native-entry-mcp";
const ADAPTER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_NATIVE_ELAPSED_SECONDS: u64 = 900;
const MAX_NATIVE_STORAGE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_NATIVE_MODEL_SPEND_CENTS: u64 = 100;
pub(crate) const NATIVE_ENTRY_INSTRUCTIONS: &str = "This is a Tyrion Entry Session. For each substantial new user task, call tyrion_start_commission exactly once with a complete proposal. When the task splits into independent parts touching different files, include a plan so Tyrion runs them in parallel. Tyrion Workers execute the accepted Commission, so do not independently perform that commissioned work in this host Entry Session. Use tyrion_status to inspect progress. If blocked work is abandoned, call tyrion_cancel_commission before starting another task. Construct proposals yourself and never ask the user to author Tyrion JSON or configure sockets, tokens, catalogs, or setup commands. One harness session may host multiple sequential Commissions. Accepted work is integrated into a Tyrion-owned repository, never the user's checkout: when reporting back, say whether the Commission reached verified_complete, and give the review commands from tyrion_status rather than presenting files as already present in the checkout.";
const ENTRY_CAPABILITIES: [&str; 3] = [
    "proposal_creation",
    "commission_acceptance",
    "commission_inspection",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHarness {
    Claude,
    Codex,
}

impl NativeHarness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl std::str::FromStr for NativeHarness {
    type Err = TyrionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(TyrionError::InvalidRequest(
                "native Entry harness must be claude or codex".into(),
            )),
        }
    }
}

pub fn run_entry_mcp(socket: &Path, harness: NativeHarness) -> Result<(), TyrionError> {
    let attachment_token = connect_entry(socket, harness)?;
    let mut state = EntryState {
        socket,
        attachment_token,
        current_commission_id: None,
        last_started: None,
        start_operation_id: Uuid::new_v4().to_string(),
    };
    let input = io::stdin();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lock().lines() {
        let line = line?;
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                write_message(
                    &mut output,
                    &json_rpc_error(Value::Null, -32700, format!("invalid JSON: {error}")),
                )?;
                continue;
            }
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message["method"].as_str().unwrap_or_default();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": negotiated_protocol_version(&message["params"]),
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "tyrion-native-entry",
                        "title": "Tyrion Commission Control",
                        "version": ADAPTER_VERSION,
                    },
                    "instructions": NATIVE_ENTRY_INSTRUCTIONS
                }
            }),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => {
                json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}})
            }
            "tools/call" => match call_tool(&mut state, &message["params"]) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": error.to_string()}],
                        "isError": true,
                    }
                }),
            },
            _ => json_rpc_error(id, -32601, format!("unknown MCP method {method}")),
        };
        write_message(&mut output, &response)?;
    }
    Ok(())
}

struct EntryState<'a> {
    socket: &'a Path,
    attachment_token: String,
    current_commission_id: Option<String>,
    last_started: Option<CachedStart>,
    start_operation_id: String,
}

struct CachedStart {
    proposal: CommissionProposal,
    result: Value,
}

pub(crate) fn connect_entry(socket: &Path, harness: NativeHarness) -> Result<String, TyrionError> {
    let adapter = AdapterIdentity {
        harness: harness.as_str().into(),
        adapter_identity: ADAPTER_IDENTITY.into(),
        adapter_version: ADAPTER_VERSION.into(),
    };
    let issued = successful_data(send_request(
        socket,
        &Request {
            protocol_version: PROTOCOL_VERSION,
            attachment_token: None,
            principal_token: None,
            idempotency_key: Some(format!("native-entry-issue-{}", Uuid::new_v4())),
            expected_revision: None,
            expected_control_revision: None,
            command: Command::IssueAttachmentToken {
                expected_adapter: adapter.clone(),
                ttl_seconds: 60,
            },
        },
    )?)?;
    let launch_token = issued["launch_token"].as_str().ok_or_else(|| {
        TyrionError::AttachmentRejected("launch token response was incomplete".into())
    })?;
    let connected = successful_data(send_request(
        socket,
        &Request {
            protocol_version: PROTOCOL_VERSION,
            attachment_token: None,
            principal_token: None,
            idempotency_key: Some(format!("native-entry-connect-{}", Uuid::new_v4())),
            expected_revision: None,
            expected_control_revision: None,
            command: Command::ConnectAttachment {
                launch_token: launch_token.to_owned(),
                handshake: Box::new(AttachmentHandshake {
                    adapter,
                    adapter_protocol_version: PROTOCOL_VERSION,
                    native_session_id: format!("native-entry-{}", Uuid::new_v4()),
                    capabilities: ENTRY_CAPABILITIES
                        .iter()
                        .map(|capability| (*capability).to_owned())
                        .collect(),
                }),
                replay: None,
            },
        },
    )?)?;
    connected["attachment_session_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            TyrionError::AttachmentRejected(
                "Attachment connection response omitted its session token".into(),
            )
        })
}

fn call_tool(state: &mut EntryState<'_>, params: &Value) -> Result<Value, TyrionError> {
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match params["name"].as_str().unwrap_or_default() {
        "tyrion_start_commission" => start_commission(state, &arguments),
        "tyrion_status" => commission_status(state, &arguments),
        "tyrion_export_record" => export_record(state, &arguments),
        "tyrion_cancel_commission" => cancel_commission(state),
        name => Err(TyrionError::InvalidRequest(format!(
            "unknown Tyrion Entry tool {name}"
        ))),
    }
}

fn start_commission(state: &mut EntryState<'_>, arguments: &Value) -> Result<Value, TyrionError> {
    let proposal: CommissionProposal = serde_json::from_value(
        arguments
            .get("proposal")
            .cloned()
            .ok_or_else(|| TyrionError::InvalidRequest("proposal is required".into()))?,
    )?;
    if let Some(cached) = state
        .last_started
        .as_ref()
        .filter(|cached| cached.proposal == proposal)
    {
        return Ok(cached.result.clone());
    }
    validate_native_entry_proposal(&proposal)?;
    ensure_previous_commission_is_terminal(state)?;
    if state.last_started.take().is_some() {
        state.start_operation_id = Uuid::new_v4().to_string();
    }
    let proposal_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&proposal)?));
    let created = authenticated_data(
        state,
        Command::CreateProposal {
            proposal: Box::new(proposal.clone()),
        },
        Some(format!(
            "native-entry-{}-propose-{proposal_digest}",
            state.start_operation_id
        )),
        None,
    )?;
    let commission_id = created["commission"]["id"]
        .as_str()
        .ok_or_else(|| {
            TyrionError::InvalidRequest("proposal response omitted its Commission id".into())
        })?
        .to_owned();
    let revision = created["commission"]["revision"].as_i64().ok_or_else(|| {
        TyrionError::InvalidRequest("proposal response omitted its Commission revision".into())
    })?;
    let accepted = authenticated_data(
        state,
        Command::AcceptCommission {
            commission_id: commission_id.clone(),
        },
        Some(format!(
            "native-entry-{}-accept-{commission_id}",
            state.start_operation_id
        )),
        Some(revision),
    )?;
    state.current_commission_id = Some(commission_id);
    // The acceptance response is the full projection; answer with the same
    // digest status uses, so starting a large plan does not flood the host.
    let result = tool_result(if accepted["assignments"].is_array() {
        crate::digest::commission_digest(&accepted)
    } else {
        accepted
    });
    state.last_started = Some(CachedStart {
        proposal,
        result: result.clone(),
    });
    Ok(result)
}

fn ensure_previous_commission_is_terminal(state: &mut EntryState<'_>) -> Result<(), TyrionError> {
    let Some(commission_id) = state.current_commission_id.clone() else {
        return Ok(());
    };
    let inspected = authenticated_data(
        state,
        Command::InspectCommission {
            commission_id: commission_id.clone(),
        },
        None,
        None,
    )?;
    let status = inspected["commission"]["status"].as_str().ok_or_else(|| {
        TyrionError::InvalidRequest("inspection omitted Commission status".into())
    })?;
    if matches!(status, "verified_complete" | "cancelled") {
        state.current_commission_id = None;
        return Ok(());
    }
    Err(TyrionError::InvalidRequest(format!(
        "this Entry Session already has a non-terminal Commission {commission_id}; use tyrion_status before starting another task"
    )))
}

/// How many Workers run at once is not bounded here: the daemon admits them
/// against the machine's measured capacity, across every Commission. What an
/// Entry Session may not do is reach outside the repository or ask for work
/// that needs a human or a model to judge.
fn validate_native_entry_proposal(proposal: &CommissionProposal) -> Result<(), TyrionError> {
    if proposal.plan.as_ref().is_some_and(|plan| {
        plan.assignments
            .iter()
            .any(|assignment| assignment.competition.is_some())
    }) {
        return Err(TyrionError::InvalidRequest(
            "native Entry does not run competing Attempts; give each Assignment its own work"
                .into(),
        ));
    }
    if proposal.resource_ceilings.max_elapsed_seconds > MAX_NATIVE_ELAPSED_SECONDS {
        return Err(TyrionError::InvalidRequest(format!(
            "native Entry MVP limits max_elapsed_seconds to {MAX_NATIVE_ELAPSED_SECONDS}"
        )));
    }
    if proposal.resource_ceilings.max_storage_bytes > MAX_NATIVE_STORAGE_BYTES {
        return Err(TyrionError::InvalidRequest(format!(
            "native Entry MVP limits max_storage_bytes to {MAX_NATIVE_STORAGE_BYTES}"
        )));
    }
    if proposal.resource_ceilings.max_model_spend_cents > MAX_NATIVE_MODEL_SPEND_CENTS {
        return Err(TyrionError::InvalidRequest(format!(
            "native Entry MVP limits max_model_spend_cents to {MAX_NATIVE_MODEL_SPEND_CENTS}"
        )));
    }
    if proposal.resource_ceilings.max_paid_service_spend_cents != 0 {
        return Err(TyrionError::InvalidRequest(
            "native Entry MVP requires max_paid_service_spend_cents to equal 0".into(),
        ));
    }
    if !proposal.authority.destinations.is_empty() || !proposal.authority.effects.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "an Entry Session grants no external destinations or effects: leave authority.destinations and authority.effects out. Repository changes are already authorized by authority.paths and codex.git_change, so do not describe the work there".into(),
        ));
    }
    if proposal
        .authority
        .actions
        .iter()
        .any(|action| !matches!(action.as_str(), "deterministic.echo" | "codex.git_change"))
    {
        return Err(TyrionError::InvalidRequest(
            "native Entry MVP permits only deterministic.echo or codex.git_change".into(),
        ));
    }
    if proposal
        .criteria
        .iter()
        .any(|criterion| criterion.verifier_type != VerifierType::Deterministic)
    {
        return Err(TyrionError::InvalidRequest(
            "native Entry MVP permits only deterministic verifiers".into(),
        ));
    }
    Ok(())
}

fn commission_status(state: &EntryState<'_>, arguments: &Value) -> Result<Value, TyrionError> {
    let commission_id = arguments
        .get("commission_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| state.current_commission_id.clone())
        .ok_or_else(|| {
            TyrionError::InvalidRequest("no Commission is active in this session".into())
        })?;
    let inspected = authenticated_data(
        state,
        Command::InspectCommission { commission_id },
        None,
        None,
    )?;
    if arguments.get("detail").and_then(Value::as_bool) == Some(true) {
        return Ok(tool_result(inspected));
    }
    Ok(tool_result(crate::digest::commission_digest(&inspected)))
}

/// The record is the artifact that says what actually happened. Requiring the
/// Principal to leave the Entry Session and rejoin over the CLI to read it
/// would defeat the point of the Entry Session.
fn export_record(state: &EntryState<'_>, arguments: &Value) -> Result<Value, TyrionError> {
    let commission_id = arguments
        .get("commission_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| state.current_commission_id.clone())
        .ok_or_else(|| {
            TyrionError::InvalidRequest("no Commission is active in this session".into())
        })?;
    let exported = authenticated_data(
        state,
        Command::ExportCommissionRecord { commission_id },
        None,
        None,
    )?;
    Ok(tool_result(exported))
}

fn cancel_commission(state: &mut EntryState<'_>) -> Result<Value, TyrionError> {
    let commission_id = state.current_commission_id.clone().ok_or_else(|| {
        TyrionError::InvalidRequest("no Commission is active in this session".into())
    })?;
    let inspected = authenticated_data(
        state,
        Command::InspectCommission {
            commission_id: commission_id.clone(),
        },
        None,
        None,
    )?;
    let revision = inspected["commission"]["revision"]
        .as_i64()
        .ok_or_else(|| {
            TyrionError::InvalidRequest("inspection omitted Commission revision".into())
        })?;
    if inspected["commission"]["status"] == "cancelled" {
        clear_current_operation(state);
        return Ok(tool_result(inspected));
    }
    let cancelled = authenticated_data(
        state,
        Command::CancelCommission {
            commission_id: commission_id.clone(),
        },
        Some(format!(
            "native-entry-{}-cancel-{commission_id}",
            state.start_operation_id
        )),
        Some(revision),
    )?;
    clear_current_operation(state);
    Ok(tool_result(cancelled))
}

fn clear_current_operation(state: &mut EntryState<'_>) {
    state.current_commission_id = None;
    state.last_started = None;
    state.start_operation_id = Uuid::new_v4().to_string();
}

fn authenticated_data(
    state: &EntryState<'_>,
    command: Command,
    idempotency_key: Option<String>,
    expected_revision: Option<i64>,
) -> Result<Value, TyrionError> {
    successful_data(send_request(
        state.socket,
        &Request {
            protocol_version: PROTOCOL_VERSION,
            attachment_token: Some(state.attachment_token.clone()),
            principal_token: None,
            idempotency_key,
            expected_revision,
            expected_control_revision: None,
            command,
        },
    )?)
}

fn successful_data(response: crate::protocol::Response) -> Result<Value, TyrionError> {
    if response.ok {
        return response
            .data
            .ok_or_else(|| TyrionError::InvalidRequest("Tyrion response omitted data".into()));
    }
    Err(TyrionError::InvalidRequest(
        response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "Tyrion request failed".into()),
    ))
}

fn tool_result(value: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&value).expect("Value serializes"),
        }],
        "structuredContent": value,
        "isError": false,
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "tyrion_start_commission",
            "title": "Start Tyrion Commission",
            "description": "Create and accept one durable Tyrion Commission for the user's current substantial task. Construct the proposal yourself; never ask the user to author JSON or perform Tyrion setup. For repository work, use codex_git with the absolute current Git root, full HEAD object id, narrowly authorized relative paths, codex.git_change, and deterministic command verifiers. When the task has independent parts that touch different files, include a plan with one Assignment per part so Workers run them in parallel; Tyrion runs as many at once as the machine can hold. Limited to 15 minutes, 100 MiB, $1 model spend, no paid services, and no external effects.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proposal": proposal_schema()
                },
                "required": ["proposal"],
                "additionalProperties": false
            }
        },
        {
            "name": "tyrion_status",
            "title": "Inspect Tyrion Commission",
            "description": "A compact digest of the current Commission or a specified Commission id: a one-line headline, anything that needs the user first, one line per Assignment (state, Worker, elapsed time, activity, reported cost), verification, and review commands once work is integrated. Relay anything under needs_you to the user. Poll this while Workers run; pass detail: true only when you need the full record of Evidence, plans, and events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commission_id": {"type": "string"},
                    "detail": {
                        "type": "boolean",
                        "description": "Return the full record instead of the digest. Large: avoid polling with it."
                    }
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "tyrion_export_record",
            "title": "Export Tyrion Commission Record",
            "description": "Export the checksummed durable record for the current Commission or a specified Commission id: mandate, routes, Attempts, Results, Evidence, Integration, events, and the final run report. Use it to show the user what was actually done and proven.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commission_id": {"type": "string"}
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "tyrion_cancel_commission",
            "title": "Cancel Current Tyrion Commission",
            "description": "Cancel the current non-terminal Commission when the user abandons blocked or unwanted work, preserving its durable record so another task can start.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": {"destructiveHint": true}
        }
    ])
}

fn proposal_schema() -> Value {
    json!({
        "type": "object",
        "description": "Complete bounded Commission Proposal. Use one Commission per substantial user task.",
        "properties": {
            "project_id": {"type": "string", "minLength": 1},
            "commission_constraints": {
                "type": "array",
                "items": {"type": "string", "minLength": 1},
                "default": []
            },
            "goal": {"type": "string", "minLength": 1},
            "plan": plan_schema(),
            "planning": {
                "type": "string",
                "enum": ["worker"],
                "description": "Ask Tyrion to plan instead of writing a plan: a contained, read-only planning Worker reads the repository and proposes the Assignments, and Tyrion validates them before any Worker writes. Use it when you cannot see the codebase well enough to split the work yourself. Omit plan when you use this, and allow max_attempts for the planning Attempt plus the Assignments."
            },
            "execution": {
                "oneOf": [
                    {
                        "type": "object",
                        "description": "Safe built-in echo execution for smoke tests and non-repository checks.",
                        "properties": {"kind": {"const": "deterministic"}},
                        "required": ["kind"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "description": "Contained repository change from one immutable Git base.",
                        "properties": {
                            "kind": {"const": "codex_git"},
                            "repository": {
                                "type": "string",
                                "description": "Absolute path to the current Git repository."
                            },
                            "base_revision": {
                                "type": "string",
                                "pattern": "^[0-9A-Fa-f]{40,64}$",
                                "description": "Full immutable Git HEAD object id."
                            }
                        },
                        "required": ["kind", "repository", "base_revision"],
                        "additionalProperties": false
                    }
                ]
            },
            "criteria": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "minLength": 1},
                        "description": {"type": "string", "minLength": 1},
                        "required_evidence": {"type": "string", "minLength": 1},
                        "verifier_type": {"const": "deterministic"},
                        "verification_depth": {
                            "type": "string",
                            "enum": ["standard", "independent"]
                        },
                        "verifier_configuration": {"type": "string"},
                        "verification_environment": {"type": "string"},
                        "verifier": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": {"const": "exact_match"},
                                        "expected": {"type": "string"}
                                    },
                                    "required": ["kind", "expected"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": {"const": "command"},
                                        "argv": {
                                            "type": "array",
                                            "minItems": 1,
                                            "items": {"type": "string"}
                                        }
                                    },
                                    "required": ["kind", "argv"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "kind": {"const": "prompt"},
                                        "prompt": {"type": "string", "minLength": 1}
                                    },
                                    "required": ["kind", "prompt"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    "required": [
                        "id",
                        "description",
                        "required_evidence",
                        "verifier_type",
                        "verification_depth",
                        "verifier"
                    ],
                    "additionalProperties": false
                }
            },
            "authority": {
                "type": "object",
                "description": "Exact authority granted to this Commission. codex_git requires its repository, at least one normalized relative path, and codex.git_change. An Entry Session never grants network destinations or external effects, so there are no fields for them.",
                "properties": {
                    "repositories": {"type": "array", "items": {"type": "string"}},
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "actions": {
                        "type": "array",
                        "items": {"enum": ["deterministic.echo", "codex.git_change"]},
                        "uniqueItems": true
                    },
                },
                "required": ["repositories", "paths", "actions"],
                "additionalProperties": false
            },
            "resource_ceilings": {
                "type": "object",
                "properties": {
                    "max_attempts": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Attempts across every Assignment, including retries. Allow at least one per Assignment plus one retry each."
                    },
                    "max_elapsed_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_NATIVE_ELAPSED_SECONDS
                    },
                    "max_worker_concurrency": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many Assignments may run at once: the number of plan Assignments that do not depend on each other, or 1 without a plan. Tyrion further limits this to what the machine can hold."
                    },
                    "max_storage_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_NATIVE_STORAGE_BYTES
                    },
                    "max_model_spend_cents": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": MAX_NATIVE_MODEL_SPEND_CENTS
                    },
                    "max_paid_service_spend_cents": {"const": 0}
                },
                "required": [
                    "max_attempts",
                    "max_elapsed_seconds",
                    "max_worker_concurrency",
                    "max_storage_bytes",
                    "max_model_spend_cents",
                    "max_paid_service_spend_cents"
                ],
                "additionalProperties": false
            },
            "known_uncertainties": {
                "type": "array",
                "items": {"type": "string"}
            }
        },
        "required": [
            "goal",
            "execution",
            "criteria",
            "authority",
            "resource_ceilings",
            "known_uncertainties"
        ],
        "additionalProperties": false
    })
}

/// A parallel plan, in exactly the shape the daemon validates.
fn plan_schema() -> Value {
    json!({
        "type": "object",
        "description": "Split the task into Assignments that Workers run in parallel. Use a plan when the work has at least two independent parts touching different files; omit it for one focused change. Every criterion is owned by exactly one Assignment. Assignments that can run at the same time must have non-overlapping write_scopes; one that needs another's output lists it in dependencies.",
        "properties": {
            "assignments": {
                "type": "array",
                "minItems": 2,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "minLength": 1},
                        "goal": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Complete instructions for one Worker, which sees only this goal and the repository."
                        },
                        "dependencies": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Ids of Assignments whose integrated result this one needs first."
                        },
                        "criterion_ids": {
                            "type": "array",
                            "minItems": 1,
                            "items": {"type": "string"}
                        },
                        "purpose": {
                            "type": "string",
                            "enum": ["critical_path", "uncertainty_reduction", "independent_verification"]
                        },
                        "read_scopes": {"type": "array", "items": {"type": "string"}},
                        "write_scopes": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Relative paths this Assignment may change, each also listed in authority.paths. Empty for a read-only Assignment, such as one that verifies the others' integrated result."
                        },
                        "resources": {
                            "type": "object",
                            "properties": {
                                "concurrency_slots": {"const": 1},
                                "max_storage_bytes": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Room for this Assignment's Result. 5242880 suits most code changes."
                                },
                                "max_model_spend_cents": {"type": "integer", "minimum": 0},
                                "max_paid_service_spend_cents": {"const": 0}
                            },
                            "required": [
                                "concurrency_slots",
                                "max_storage_bytes",
                                "max_model_spend_cents",
                                "max_paid_service_spend_cents"
                            ],
                            "additionalProperties": false
                        }
                    },
                    "required": [
                        "id",
                        "goal",
                        "criterion_ids",
                        "purpose",
                        "write_scopes",
                        "resources"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["assignments"],
        "additionalProperties": false
    })
}

fn negotiated_protocol_version(params: &Value) -> String {
    match params["protocolVersion"].as_str() {
        Some(version @ ("2025-06-18" | "2025-03-26" | "2024-11-05")) => version.to_owned(),
        _ => "2025-06-18".into(),
    }
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<(), TyrionError> {
    serde_json::to_writer(&mut *output, message)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an instance from every property the plan schema offers. The
    /// daemon's types reject unknown fields, so a schema that names anything
    /// the daemon does not accept fails here rather than in a host model.
    #[test]
    fn plan_schema_offers_only_fields_the_daemon_accepts() {
        let schema = plan_schema();
        let assignment = &schema["properties"]["assignments"]["items"];
        let mut instance = serde_json::Map::new();
        for (name, property) in assignment["properties"].as_object().unwrap() {
            let value = match name.as_str() {
                "purpose" => json!("critical_path"),
                "resources" => json!({
                    "concurrency_slots": 1,
                    "max_storage_bytes": 5_242_880,
                    "max_model_spend_cents": 0,
                    "max_paid_service_spend_cents": 0
                }),
                _ if property["type"] == "array" => json!(["src/lib.rs"]),
                _ => json!("value"),
            };
            instance.insert(name.clone(), value);
        }
        let plan: crate::protocol::CommissionPlan = serde_json::from_value(json!({
            "assignments": [Value::Object(instance)]
        }))
        .expect("every schema property is a daemon field");
        assert_eq!(plan.assignments[0].write_scopes, ["src/lib.rs"]);
        for required in assignment["required"].as_array().unwrap() {
            assert!(assignment["properties"]
                .get(required.as_str().unwrap())
                .is_some());
        }
    }
}

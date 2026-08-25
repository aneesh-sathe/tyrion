use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::contained_codex::{ContainedCodexRuntime, StructuredGitAttempt};
use super::routing::{WorkerAdapterKind, WorkerConfiguration};
use super::{AssignmentContext, WorkerControl};
use crate::adapter_contract::{
    validate_observed_skill_invocations, validate_trace, AdapterContractExpectation,
    AdapterContractReport, StructuredAdapterKind,
};
use crate::protocol::SkillVersion;
use crate::TyrionError;

pub(super) fn execute(
    runtime: &ContainedCodexRuntime,
    configuration: &WorkerConfiguration,
    assignment: &AssignmentContext,
    control: &Arc<WorkerControl>,
    git_attempt: Option<&StructuredGitAttempt>,
) -> Result<AdapterContractReport, TyrionError> {
    configuration.adapter.command.split_first().ok_or_else(|| {
        TyrionError::InvalidRequest(format!(
            "Worker Configuration {} has no structured adapter command",
            configuration.id
        ))
    })?;
    let kind = match configuration.adapter.kind {
        WorkerAdapterKind::CodexAppServer => StructuredAdapterKind::CodexAppServer,
        WorkerAdapterKind::ClaudeAgentSdk => StructuredAdapterKind::ClaudeAgentSdk,
        _ => {
            return Err(TyrionError::InvalidRequest(
                "structured adapter runner received a non-structured configuration".into(),
            ))
        }
    };
    let configuration_fingerprint =
        format!("{:x}", Sha256::digest(serde_json::to_vec(configuration)?));
    let mut sandbox = runtime
        .prepare_structured_adapter_sandbox(configuration, assignment, git_attempt)
        .map_err(|error| configuration_unavailable(configuration, error))?;
    let mut child = sandbox
        .command(
            configuration,
            assignment,
            git_attempt,
            &configuration_fingerprint,
        )
        .map_err(|error| configuration_unavailable(configuration, error))?
        .spawn()
        .map_err(|error| configuration_unavailable(configuration, error))?;
    let process_io = (|| -> Result<_, TyrionError> {
        let mut input = child.stdin.take().ok_or_else(|| {
            TyrionError::InvalidRequest("structured Worker adapter has no command input".into())
        })?;
        serde_json::to_writer(
            &mut input,
            &json!({
                "type": "tyrion.assignment.launch",
                "commission_id": assignment.commission_id,
                "assignment_id": assignment.assignment_id,
                "attempt_id": assignment.attempt_id,
                "mandate_revision": assignment.mandate_revision,
                "plan_revision": assignment.plan_revision,
                "goal": assignment.goal,
                "execution": assignment.execution,
                "criteria": assignment.criteria,
                "authority": assignment.authority,
                "declared_write_scopes": assignment.declared_write_scopes,
                "authorized_paths": assignment.authorized_paths,
                "max_storage_bytes": assignment.max_storage_bytes,
                "resource_limits": {
                    "max_storage_bytes": assignment.max_storage_bytes,
                    "max_model_spend_cents": assignment.max_model_spend_cents,
                    "max_paid_service_spend_cents": assignment.max_paid_service_spend_cents,
                },
                "lease_expires_at": assignment.lease_expires_at,
                "worker_configuration": configuration,
                "skill_defaults": assignment.skill_defaults,
                "configuration_fingerprint": configuration_fingerprint,
                "git_artifacts": git_attempt.map(StructuredGitAttempt::launch_payload),
            }),
        )?;
        input.write_all(b"\n")?;
        input.flush()?;
        control.attach_adapter_input(input)?;

        let stdout = child.stdout.take().ok_or_else(|| {
            TyrionError::InvalidRequest("structured Worker adapter has no event output".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            TyrionError::InvalidRequest("structured Worker adapter has no error output".into())
        })?;
        Ok((stdout, stderr))
    })();
    let (stdout, stderr) = match process_io {
        Ok(process_io) => process_io,
        Err(error) => {
            let _ = control.detach_adapter_input();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let event_output_limit = assignment.max_storage_bytes;
    let event_control = Arc::clone(control);
    let containment_profile = configuration.containment_profile.clone();
    let event_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout).take(event_output_limit.saturating_add(1));
        let mut total_bytes = 0_u64;
        let mut events = Vec::new();
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            total_bytes = total_bytes.saturating_add(bytes as u64);
            if total_bytes > event_output_limit {
                return Err(std::io::Error::other(
                    "structured Worker adapter event output exceeded max_storage_bytes",
                ));
            }
            let mut event = serde_json::from_str::<Value>(&line).map_err(std::io::Error::other)?;
            if events.is_empty() && event["type"] == "tyrion.adapter.ready" {
                let ready = event.as_object_mut().ok_or_else(|| {
                    std::io::Error::other("structured adapter ready event must be an object")
                })?;
                ready.insert("containment_enforced".into(), Value::Bool(true));
                ready.insert(
                    "containment_profile".into(),
                    Value::String(containment_profile.clone()),
                );
            }
            event_control
                .observe_adapter_event(configuration_kind(kind), &event)
                .map_err(std::io::Error::other)?;
            events.push(event);
        }
        Ok(events)
    });
    let error_reader = thread::spawn(move || {
        let mut output = String::new();
        stderr
            .take(65_536)
            .read_to_string(&mut output)
            .map(|_| output)
    });

    let mut interrupted_at = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if unix_timestamp()? >= assignment.lease_expires_at {
            sandbox.terminate();
            child.kill()?;
            let _ = child.wait();
            control.detach_adapter_input()?;
            return Err(TyrionError::WorkerLeaseExpired {
                operation: "structured adapter execution",
            });
        }
        if control.was_interrupted() {
            let started = interrupted_at.get_or_insert_with(Instant::now);
            if started.elapsed() >= Duration::from_secs(2) {
                sandbox.terminate();
                child.kill()?;
            }
        }
        thread::sleep(Duration::from_millis(20));
    };
    control.detach_adapter_input()?;
    if status.success() {
        sandbox.finish(git_attempt, assignment.lease_expires_at)?;
    } else {
        sandbox.terminate();
    }
    let events = event_reader
        .join()
        .map_err(|_| TyrionError::InvalidRequest("adapter event reader panicked".into()))??;
    let stderr = error_reader
        .join()
        .map_err(|_| TyrionError::InvalidRequest("adapter error reader panicked".into()))??;
    if events
        .first()
        .is_some_and(|event| event["type"] == "tyrion.adapter.ready")
    {
        let observed = validate_observed_skill_invocations(&events, &configuration.skills)?;
        control.record_validated_skill_versions(&observed)?;
    }
    if !status.success() {
        if let Some(failure) = events.iter().find(|event| {
            event["type"] == "tyrion.adapter.unavailable"
                && event["code"] == "required_skill_failure"
        }) {
            let skill_name = failure["skill"]["name"].as_str().ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "required Skill failure report is missing its Skill name".into(),
                )
            })?;
            let content_digest = failure["skill"]["content_digest"].as_str().ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "required Skill failure report is missing its content identity".into(),
                )
            })?;
            let pinned = assignment
                .skill_defaults
                .iter()
                .any(|skill| skill.name == skill_name && skill.content_digest == content_digest);
            if !pinned {
                return Err(TyrionError::InvalidRequest(
                    "required Skill failure report does not match an exact pinned Assignment default"
                        .into(),
                ));
            }
            let message = failure["message"]
                .as_str()
                .filter(|message| !message.is_empty())
                .ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "required Skill failure report is missing its message".into(),
                    )
                })?;
            return Err(TyrionError::RequiredSkillUnavailable {
                configuration_id: configuration.id.clone(),
                skill_name: skill_name.to_owned(),
                content_digest: content_digest.to_owned(),
                message: message.to_owned(),
            });
        }
        let message = format!(
            "structured Worker adapter {} exited with {status}: {}",
            configuration.id,
            stderr.trim()
        );
        if !events
            .iter()
            .any(|event| event["type"] == "tyrion.adapter.ready")
        {
            return Err(configuration_unavailable(configuration, message));
        }
        return Err(TyrionError::InvalidRequest(message));
    }
    let expected_skills = assignment
        .skill_defaults
        .iter()
        .map(|skill| SkillVersion {
            name: skill.name.clone(),
            content_digest: skill.content_digest.clone(),
        })
        .collect::<Vec<_>>();
    let report = validate_trace(
        kind,
        &events,
        AdapterContractExpectation {
            configuration_id: &configuration.id,
            containment_profile: &configuration.containment_profile,
            expected_skills: &expected_skills,
            allowed_skills: &configuration.skills,
            commission_id: &assignment.commission_id,
            assignment_id: &assignment.assignment_id,
            attempt_id: &assignment.attempt_id,
            configuration_fingerprint: &configuration_fingerprint,
            mandate_revision: assignment.mandate_revision,
            plan_revision: assignment.plan_revision,
        },
    )
    .map_err(|error| {
        if events
            .iter()
            .any(|event| event["type"] == "tyrion.adapter.ready")
        {
            error
        } else {
            configuration_unavailable(configuration, error)
        }
    })?;
    match report.terminal_state.as_str() {
        "completed" => Ok(report),
        "interrupted" => Err(TyrionError::WorkerInterrupted),
        "failed" => Err(TyrionError::InvalidRequest(format!(
            "structured Worker adapter {} reported failure",
            configuration.id
        ))),
        _ => unreachable!("adapter contract validates terminal states"),
    }
}

fn configuration_unavailable(
    configuration: &WorkerConfiguration,
    error: impl std::fmt::Display,
) -> TyrionError {
    TyrionError::WorkerConfigurationUnavailable {
        configuration_id: configuration.id.clone(),
        message: error.to_string(),
    }
}

const fn configuration_kind(kind: StructuredAdapterKind) -> WorkerAdapterKind {
    match kind {
        StructuredAdapterKind::CodexAppServer => WorkerAdapterKind::CodexAppServer,
        StructuredAdapterKind::ClaudeAgentSdk => WorkerAdapterKind::ClaudeAgentSdk,
    }
}

fn unix_timestamp() -> Result<i64, TyrionError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TyrionError::InvalidRequest("system clock is before Unix epoch".into()))?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| TyrionError::InvalidRequest("system time does not fit in i64".into()))
}

use serde_json::Value;

use crate::protocol::SkillVersion;
use crate::TyrionError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuredAdapterKind {
    CodexAppServer,
    ClaudeAgentSdk,
    PiRpc,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AdapterContractReport {
    pub native_session_id: String,
    pub lifecycle_started: bool,
    pub interrupted: bool,
    pub terminal_state: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub native_skills: Vec<String>,
    pub skill_versions: Vec<SkillVersion>,
    pub cost_cents: u64,
    pub containment_profile: String,
    pub result_summary: String,
    pub latest_meaningful_activity: String,
}

#[derive(Debug)]
pub struct AdapterContractExpectation<'a> {
    pub configuration_id: &'a str,
    pub containment_profile: &'a str,
    pub expected_skills: &'a [SkillVersion],
    pub allowed_skills: &'a [SkillVersion],
    pub commission_id: &'a str,
    pub assignment_id: &'a str,
    pub attempt_id: &'a str,
    pub configuration_fingerprint: &'a str,
    pub mandate_revision: i64,
    pub plan_revision: i64,
}

pub fn validate_trace(
    kind: StructuredAdapterKind,
    trace: &[Value],
    expectation: AdapterContractExpectation<'_>,
) -> Result<AdapterContractReport, TyrionError> {
    let ready = trace
        .first()
        .filter(|event| event["type"] == "tyrion.adapter.ready")
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "structured Worker adapter trace must begin with tyrion.adapter.ready".into(),
            )
        })?;
    if trace
        .iter()
        .filter(|event| event["type"] == "tyrion.adapter.ready")
        .count()
        != 1
    {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter must emit exactly one tyrion.adapter.ready".into(),
        ));
    }
    let native_session_id = required_string(ready, "native_session_id")?;
    if ready["configuration_fingerprint"].as_str() != Some(expectation.configuration_fingerprint) {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter did not confirm the exact selected configuration".into(),
        ));
    }
    let containment_profile = required_string(ready, "containment_profile")?;
    if ready["containment_enforced"] != true {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter did not attest enforced containment".into(),
        ));
    }
    if containment_profile != expectation.containment_profile {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter containment profile does not match its selected configuration"
                .into(),
        ));
    }
    let native_skills = native_skill_names(ready)?;
    if let Some(missing) = expectation
        .expected_skills
        .iter()
        .find(|required| !native_skills.contains(&required.name))
    {
        return Err(required_skill_unavailable(
            &expectation,
            missing,
            "structured Worker adapter did not load the pinned native Skill",
        ));
    }
    let native_skill_preparations =
        ready["native_skill_preparations"]
            .as_array()
            .ok_or_else(|| {
                expectation.expected_skills.first().map_or_else(
                    || {
                        TyrionError::InvalidRequest(
                            "structured Worker adapter did not report native Skill preparations"
                                .into(),
                        )
                    },
                    |skill| {
                        required_skill_unavailable(
                            &expectation,
                            skill,
                            "structured Worker adapter did not prepare the pinned native Skill",
                        )
                    },
                )
            })?;
    let mut prepared_versions = Vec::new();
    for preparation in native_skill_preparations {
        let name = required_string(preparation, "name")?;
        let content_digest = required_string(preparation, "content_digest")?;
        required_string(preparation, "source")?;
        let version = SkillVersion {
            name: name.clone(),
            content_digest,
        };
        if !version.is_content_identified() {
            if let Some(expected) = expectation
                .expected_skills
                .iter()
                .find(|expected| expected.name == name)
            {
                return Err(required_skill_unavailable(
                    &expectation,
                    expected,
                    "structured Worker adapter reported an invalid content identity for the pinned native Skill",
                ));
            }
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter reported an invalid content identity for Skill {name}"
            )));
        }
        if !expectation.allowed_skills.contains(&version) {
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter prepared Skill {name} outside the selected capability inventory"
            )));
        }
        if prepared_versions.iter().any(|prior| prior == &version) {
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter reported duplicate preparation for Skill {name}"
            )));
        }
        prepared_versions.push(version);
    }
    if prepared_versions != expectation.expected_skills {
        if let Some(missing) = expectation
            .expected_skills
            .iter()
            .find(|expected| !prepared_versions.contains(expected))
        {
            return Err(required_skill_unavailable(
                &expectation,
                missing,
                "structured Worker adapter did not prepare the exact pinned native Skill Version",
            ));
        }
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter prepared unexpected native Skill Versions".into(),
        ));
    }
    let invoked_versions =
        invoked_skill_versions(trace, &native_skills, expectation.allowed_skills)?;
    if let Some(missing) = expectation
        .expected_skills
        .iter()
        .find(|expected| !invoked_versions.contains(expected))
    {
        return Err(required_skill_unavailable(
            &expectation,
            missing,
            "structured Worker adapter did not invoke the exact pinned native Skill Version",
        ));
    }
    let typed_results = trace
        .iter()
        .filter(|event| event["type"] == "tyrion.result")
        .collect::<Vec<_>>();
    if typed_results.len() > 1 {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter emitted more than one typed Result".into(),
        ));
    }
    let typed_result = typed_results.first().copied();
    let typed_result = typed_result
        .map(|event| validate_result(event, &expectation))
        .transpose()?;
    let result_summary = typed_result.as_ref().map(|result| result.0.clone());
    let cost_cents = typed_result.as_ref().map_or(0, |result| result.1);
    let vendor_events = trace
        .iter()
        .filter(|event| {
            event["type"] != "tyrion.adapter.ready"
                && event["type"] != "tyrion.skill.invoked"
                && event["type"] != "tyrion.result"
        })
        .collect::<Vec<_>>();
    let mut lifecycle = match kind {
        StructuredAdapterKind::CodexAppServer => codex_lifecycle(&vendor_events)?,
        StructuredAdapterKind::ClaudeAgentSdk => claude_lifecycle(&vendor_events)?,
        StructuredAdapterKind::PiRpc => pi_lifecycle(&vendor_events)?,
    };
    validate_lifecycle_order(kind, &vendor_events)?;
    if let Some(summary) = result_summary {
        lifecycle.result_summary = summary;
    }
    if !lifecycle.started {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter never entered a running lifecycle state".into(),
        ));
    }
    if !matches!(
        lifecycle.terminal_state.as_str(),
        "completed" | "interrupted" | "failed"
    ) {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter emitted an invalid terminal state".into(),
        ));
    }
    if lifecycle.interrupted != (lifecycle.terminal_state == "interrupted") {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter interruption disagrees with its terminal state".into(),
        ));
    }
    if !lifecycle.usage_reported {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter emitted no usage".into(),
        ));
    }
    if lifecycle.terminal_state == "completed" && lifecycle.result_summary.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "completed structured Worker adapter emitted no typed Result summary".into(),
        ));
    }
    Ok(AdapterContractReport {
        native_session_id,
        lifecycle_started: lifecycle.started,
        interrupted: lifecycle.interrupted,
        terminal_state: lifecycle.terminal_state,
        input_tokens: lifecycle.input_tokens,
        output_tokens: lifecycle.output_tokens,
        native_skills,
        skill_versions: invoked_versions,
        cost_cents,
        containment_profile,
        result_summary: lifecycle.result_summary,
        latest_meaningful_activity: lifecycle.latest_activity,
    })
}

pub(crate) fn validate_observed_skill_invocations(
    trace: &[Value],
    allowed_skills: &[SkillVersion],
) -> Result<Vec<SkillVersion>, TyrionError> {
    let ready = trace
        .first()
        .filter(|event| event["type"] == "tyrion.adapter.ready")
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "structured Worker adapter trace must begin with tyrion.adapter.ready".into(),
            )
        })?;
    invoked_skill_versions(trace, &native_skill_names(ready)?, allowed_skills)
}

fn native_skill_names(ready: &Value) -> Result<Vec<String>, TyrionError> {
    ready["native_skills"]
        .as_array()
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "structured Worker adapter did not report native Skills".into(),
            )
        })?
        .iter()
        .map(|skill| {
            skill.as_str().map(str::to_owned).ok_or_else(|| {
                TyrionError::InvalidRequest("native Skill names must be strings".into())
            })
        })
        .collect()
}

fn invoked_skill_versions(
    trace: &[Value],
    native_skills: &[String],
    allowed_skills: &[SkillVersion],
) -> Result<Vec<SkillVersion>, TyrionError> {
    let mut invoked_versions = Vec::new();
    for invocation in trace
        .iter()
        .filter(|event| event["type"] == "tyrion.skill.invoked")
    {
        let name = required_string(invocation, "name")?;
        let content_digest = required_string(invocation, "content_digest")?;
        required_string(invocation, "source")?;
        let version = SkillVersion {
            name: name.clone(),
            content_digest,
        };
        if !version.is_content_identified() {
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter reported an invalid content identity for invoked Skill {name}"
            )));
        }
        if !allowed_skills.contains(&version) {
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter invoked Skill {name} outside the selected capability inventory"
            )));
        }
        if !native_skills.contains(&name) {
            return Err(TyrionError::InvalidRequest(format!(
                "structured Worker adapter invoked undiscovered native Skill {name}"
            )));
        }
        if let Some(prior) = invoked_versions
            .iter()
            .find(|prior: &&SkillVersion| prior.name == name)
        {
            if *prior != version {
                return Err(TyrionError::InvalidRequest(format!(
                    "structured Worker adapter invoked conflicting Versions of Skill {name}"
                )));
            }
            continue;
        }
        invoked_versions.push(version);
    }
    Ok(invoked_versions)
}

fn required_skill_unavailable(
    expectation: &AdapterContractExpectation<'_>,
    skill: &SkillVersion,
    message: &str,
) -> TyrionError {
    TyrionError::RequiredSkillUnavailable {
        configuration_id: expectation.configuration_id.to_owned(),
        skill_name: skill.name.clone(),
        content_digest: skill.content_digest.clone(),
        message: message.into(),
    }
}

fn validate_lifecycle_order(
    kind: StructuredAdapterKind,
    events: &[&Value],
) -> Result<(), TyrionError> {
    let is_start = |event: &&Value| match kind {
        StructuredAdapterKind::CodexAppServer => event["method"] == "turn/started",
        StructuredAdapterKind::ClaudeAgentSdk => event["type"] == "session.status_running",
        StructuredAdapterKind::PiRpc => event["type"] == "agent_start",
    };
    let is_usage = |event: &&Value| match kind {
        StructuredAdapterKind::CodexAppServer => event["method"] == "thread/tokenUsage/updated",
        StructuredAdapterKind::ClaudeAgentSdk => event["type"] == "span.model_request_end",
        StructuredAdapterKind::PiRpc => event["type"] == "tyrion.pi.usage",
    };
    let is_terminal = |event: &&Value| match kind {
        StructuredAdapterKind::CodexAppServer => {
            event["method"] == "turn/completed" || event["method"] == "error"
        }
        StructuredAdapterKind::ClaudeAgentSdk => {
            event["type"] == "session.status_idle" || event["type"] == "session.error"
        }
        StructuredAdapterKind::PiRpc => event["type"] == "agent_settled",
    };
    let starts = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| is_start(event).then_some(index))
        .collect::<Vec<_>>();
    let terminals = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| is_terminal(event).then_some(index))
        .collect::<Vec<_>>();
    if starts.len() != 1 || terminals.len() != 1 {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter must emit exactly one lifecycle start and terminal event"
                .into(),
        ));
    }
    let started_at = starts[0];
    let terminal_at = terminals[0];
    if started_at >= terminal_at {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter terminal state preceded its running state".into(),
        ));
    }
    if events
        .iter()
        .enumerate()
        .filter(|(_, event)| is_usage(event))
        .any(|(index, _)| index <= started_at || index >= terminal_at)
    {
        return Err(TyrionError::InvalidRequest(
            "structured Worker adapter usage must be reported while the Worker is running".into(),
        ));
    }
    Ok(())
}

fn validate_result(
    result: &Value,
    expectation: &AdapterContractExpectation<'_>,
) -> Result<(String, u64), TyrionError> {
    let string_bindings = [
        ("commission_id", expectation.commission_id),
        ("assignment_id", expectation.assignment_id),
        ("attempt_id", expectation.attempt_id),
    ];
    if string_bindings
        .iter()
        .any(|(field, expected)| result[*field].as_str() != Some(*expected))
        || result["mandate_revision"].as_i64() != Some(expectation.mandate_revision)
        || result["plan_revision"].as_i64() != Some(expectation.plan_revision)
    {
        return Err(TyrionError::InvalidRequest(
            "structured Worker Result does not match its Commission, Assignment, Attempt, or governing revisions"
                .into(),
        ));
    }
    let known_effects = result["known_effects"].as_array().ok_or_else(|| {
        TyrionError::InvalidRequest(
            "structured Worker Result must report known_effects as an array".into(),
        )
    })?;
    if !known_effects.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "structured Worker Result reported unauthorized external effects".into(),
        ));
    }
    Ok((
        required_string(result, "summary")?,
        unsigned(result, "cost_cents")?,
    ))
}

#[derive(Default)]
struct Lifecycle {
    started: bool,
    interrupted: bool,
    terminal_state: String,
    input_tokens: u64,
    output_tokens: u64,
    usage_reported: bool,
    result_summary: String,
    latest_activity: String,
}

fn codex_lifecycle(events: &[&Value]) -> Result<Lifecycle, TyrionError> {
    let mut lifecycle = Lifecycle::default();
    for event in events {
        match event["method"].as_str() {
            Some("turn/started") => {
                lifecycle.started = true;
                lifecycle.latest_activity = "Codex turn started".into();
            }
            Some("item/completed") => {
                if event["params"]["item"]["text"].as_str().is_some() {
                    lifecycle.latest_activity = "Codex produced a structured Result".into();
                }
            }
            Some("thread/tokenUsage/updated") => {
                let total = &event["params"]["tokenUsage"]["total"];
                let input_tokens = unsigned(total, "inputTokens")?;
                let output_tokens = unsigned(total, "outputTokens")?;
                if lifecycle.usage_reported
                    && (input_tokens < lifecycle.input_tokens
                        || output_tokens < lifecycle.output_tokens)
                {
                    return Err(TyrionError::InvalidRequest(
                        "Codex cumulative token usage moved backwards".into(),
                    ));
                }
                lifecycle.input_tokens = input_tokens;
                lifecycle.output_tokens = output_tokens;
                lifecycle.usage_reported = true;
            }
            Some("turn/completed") => {
                let status = required_nested_string(event, &["params", "turn", "status"])?;
                lifecycle.interrupted = status == "interrupted";
                lifecycle.terminal_state = status;
            }
            Some("error") => {
                lifecycle.terminal_state = "failed".into();
                lifecycle.latest_activity = "Codex adapter reported an error".into();
            }
            _ => {}
        }
    }
    Ok(lifecycle)
}

fn claude_lifecycle(events: &[&Value]) -> Result<Lifecycle, TyrionError> {
    let mut lifecycle = Lifecycle::default();
    for event in events {
        match event["type"].as_str() {
            Some("session.status_running") => {
                lifecycle.started = true;
                lifecycle.latest_activity = "Claude session started".into();
            }
            Some("agent.message") => {
                if event["content"]
                    .as_array()
                    .and_then(|content| content.iter().find_map(|item| item["text"].as_str()))
                    .is_some()
                {
                    lifecycle.latest_activity = "Claude produced a structured Result".into();
                }
            }
            Some("span.model_request_end") => {
                lifecycle.input_tokens = lifecycle
                    .input_tokens
                    .checked_add(unsigned(&event["usage"], "input_tokens")?)
                    .ok_or_else(|| {
                        TyrionError::InvalidRequest("Claude input token usage overflowed".into())
                    })?;
                lifecycle.output_tokens = lifecycle
                    .output_tokens
                    .checked_add(unsigned(&event["usage"], "output_tokens")?)
                    .ok_or_else(|| {
                        TyrionError::InvalidRequest("Claude output token usage overflowed".into())
                    })?;
                lifecycle.usage_reported = true;
            }
            Some("user.interrupt") => lifecycle.interrupted = true,
            Some("session.status_idle") => {
                lifecycle.terminal_state = if lifecycle.interrupted {
                    "interrupted".into()
                } else {
                    "completed".into()
                };
            }
            Some("session.error") => {
                lifecycle.terminal_state = "failed".into();
                lifecycle.latest_activity = "Claude adapter reported an error".into();
            }
            _ => {}
        }
    }
    Ok(lifecycle)
}

fn pi_lifecycle(events: &[&Value]) -> Result<Lifecycle, TyrionError> {
    let mut lifecycle = Lifecycle::default();
    for event in events {
        match event["type"].as_str() {
            Some("agent_start") => {
                lifecycle.started = true;
                lifecycle.latest_activity = "Pi agent started".into();
            }
            Some("message_end") if event["message"]["role"] == "assistant" => {
                if event["message"]["content"]
                    .as_array()
                    .and_then(|content| content.iter().find_map(|item| item["text"].as_str()))
                    .is_some()
                {
                    lifecycle.latest_activity = "Pi produced a structured Result".into();
                }
            }
            Some("tyrion.pi.usage") => {
                if lifecycle.usage_reported {
                    return Err(TyrionError::InvalidRequest(
                        "Pi adapter emitted more than one authoritative usage report".into(),
                    ));
                }
                lifecycle.input_tokens = unsigned(event, "input_tokens")?;
                lifecycle.output_tokens = unsigned(event, "output_tokens")?;
                if unsigned(event, "cost")? != 0 {
                    return Err(TyrionError::InvalidRequest(
                        "Pi adapter reported model spend under a zero-spend configuration".into(),
                    ));
                }
                lifecycle.usage_reported = true;
            }
            Some("tyrion.pi.interrupt") => lifecycle.interrupted = true,
            Some("extension_error") => {
                lifecycle.terminal_state = "failed".into();
                lifecycle.latest_activity = "Pi adapter reported an error".into();
            }
            Some("agent_settled") => {
                if lifecycle.terminal_state != "failed" {
                    lifecycle.terminal_state = if lifecycle.interrupted {
                        "interrupted".into()
                    } else {
                        "completed".into()
                    };
                }
            }
            _ => {}
        }
    }
    Ok(lifecycle)
}

fn required_string(value: &Value, field: &str) -> Result<String, TyrionError> {
    value[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(format!(
                "structured Worker adapter field {field} must be a non-empty string"
            ))
        })
}

fn required_nested_string(value: &Value, path: &[&str]) -> Result<String, TyrionError> {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(format!(
                "structured Worker adapter field {} must be a non-empty string",
                path.join(".")
            ))
        })
}

fn unsigned(value: &Value, field: &str) -> Result<u64, TyrionError> {
    value[field].as_u64().ok_or_else(|| {
        TyrionError::InvalidRequest(format!(
            "structured Worker adapter usage field {field} must be an unsigned integer"
        ))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ready(session: &str) -> Value {
        json!({
            "type": "tyrion.adapter.ready",
            "native_session_id": session,
            "native_skills": ["code-review"],
            "native_skill_preparations": [{
                "name": "code-review",
                "content_digest": format!("sha256:{}", "1".repeat(64)),
                "source": "fixture"
            }],
            "configuration_fingerprint": "configuration-sha256",
            "containment_enforced": true,
            "containment_profile": "openshell-repaired-v0.0.104"
        })
    }

    fn invoked() -> Value {
        json!({
            "type": "tyrion.skill.invoked",
            "name": "code-review",
            "content_digest": format!("sha256:{}", "1".repeat(64)),
            "source": "fixture"
        })
    }

    fn expectation() -> AdapterContractExpectation<'static> {
        static SKILLS: std::sync::LazyLock<Vec<SkillVersion>> = std::sync::LazyLock::new(|| {
            vec![SkillVersion {
                name: "code-review".into(),
                content_digest: format!("sha256:{}", "1".repeat(64)),
            }]
        });
        AdapterContractExpectation {
            configuration_id: "codex-deep",
            containment_profile: "openshell-repaired-v0.0.104",
            expected_skills: &SKILLS,
            allowed_skills: &SKILLS,
            commission_id: "commission-1",
            assignment_id: "assignment-1",
            attempt_id: "attempt-1",
            configuration_fingerprint: "configuration-sha256",
            mandate_revision: 1,
            plan_revision: 1,
        }
    }

    fn result(summary: &str) -> Value {
        json!({
            "type": "tyrion.result",
            "commission_id": "commission-1",
            "assignment_id": "assignment-1",
            "attempt_id": "attempt-1",
            "mandate_revision": 1,
            "plan_revision": 1,
            "summary": summary,
            "known_effects": [],
            "cost_cents": 0,
        })
    }

    #[test]
    fn codex_app_server_passes_the_shared_worker_adapter_contract() {
        let report = validate_trace(
            StructuredAdapterKind::CodexAppServer,
            &[
                ready("codex-thread-1"),
                invoked(),
                json!({"jsonrpc":"2.0", "method":"turn/started", "params":{"turn":{"id":"turn-1", "status":"inProgress"}}}),
                json!({"jsonrpc":"2.0", "method":"item/completed", "params":{"item":{"type":"agentMessage", "text":"implemented backend"}}}),
                json!({"jsonrpc":"2.0", "method":"thread/tokenUsage/updated", "params":{"tokenUsage":{"total":{"inputTokens":120, "outputTokens":45}}}}),
                json!({"jsonrpc":"2.0", "method":"turn/completed", "params":{"turn":{"id":"turn-1", "status":"completed"}}}),
                result("implemented backend"),
            ],
            expectation(),
        )
        .unwrap();
        assert!(report.lifecycle_started);
        assert_eq!(report.terminal_state, "completed");
        assert_eq!(report.native_session_id, "codex-thread-1");
        assert_eq!(report.input_tokens, 120);
        assert_eq!(report.output_tokens, 45);
        assert_eq!(report.native_skills, ["code-review"]);
        assert_eq!(report.result_summary, "implemented backend");
        assert_eq!(report.containment_profile, "openshell-repaired-v0.0.104");
    }

    #[test]
    fn claude_agent_sdk_passes_the_shared_worker_adapter_contract() {
        let report = validate_trace(
            StructuredAdapterKind::ClaudeAgentSdk,
            &[
                ready("claude-session-1"),
                invoked(),
                json!({"type":"session.status_running"}),
                json!({"type":"agent.message", "content":[{"type":"text", "text":"implemented frontend"}]}),
                json!({"type":"span.model_request_end", "usage":{"input_tokens":100, "output_tokens":40}}),
                json!({"type":"session.status_idle", "stop_reason":"end_turn"}),
                result("implemented frontend"),
            ],
            expectation(),
        )
        .unwrap();
        assert!(report.lifecycle_started);
        assert_eq!(report.terminal_state, "completed");
        assert_eq!(report.native_session_id, "claude-session-1");
        assert_eq!(report.input_tokens, 100);
        assert_eq!(report.output_tokens, 40);
        assert_eq!(report.native_skills, ["code-review"]);
        assert_eq!(report.result_summary, "implemented frontend");
        assert_eq!(
            report.latest_meaningful_activity,
            "Claude produced a structured Result"
        );
    }

    #[test]
    fn pi_rpc_passes_the_shared_worker_adapter_contract() {
        let report = validate_trace(
            StructuredAdapterKind::PiRpc,
            &[
                ready("pi-session-1"),
                invoked(),
                json!({"type":"agent_start"}),
                json!({
                    "type":"message_end",
                    "message": {
                        "role":"assistant",
                        "content":[{"type":"text", "text":"implemented Pi adapter"}],
                        "usage": {
                            "input":90,
                            "output":35,
                            "cost":{"total":0.0}
                        }
                    }
                }),
                json!({"type":"tyrion.pi.usage", "input_tokens":120, "output_tokens":44, "cost":0}),
                json!({"type":"agent_settled"}),
                result("implemented Pi adapter"),
            ],
            expectation(),
        )
        .unwrap();
        assert!(report.lifecycle_started);
        assert_eq!(report.terminal_state, "completed");
        assert_eq!(report.native_session_id, "pi-session-1");
        assert_eq!(report.input_tokens, 120);
        assert_eq!(report.output_tokens, 44);
        assert_eq!(report.native_skills, ["code-review"]);
        assert_eq!(report.result_summary, "implemented Pi adapter");
        assert_eq!(
            report.latest_meaningful_activity,
            "Pi produced a structured Result"
        );
    }

    #[test]
    fn both_adapters_preserve_semantic_interruption_as_terminal_state() {
        let codex = validate_trace(
            StructuredAdapterKind::CodexAppServer,
            &[
                ready("codex-thread-2"),
                invoked(),
                json!({"method":"turn/started", "params":{}}),
                json!({"method":"item/completed", "params":{"item":{"text":"partial"}}}),
                json!({"method":"thread/tokenUsage/updated", "params":{"tokenUsage":{"total":{"inputTokens":1, "outputTokens":1}}}}),
                json!({"method":"turn/completed", "params":{"turn":{"status":"interrupted"}}}),
            ],
            expectation(),
        )
        .unwrap();
        let claude = validate_trace(
            StructuredAdapterKind::ClaudeAgentSdk,
            &[
                ready("claude-session-2"),
                invoked(),
                json!({"type":"session.status_running"}),
                json!({"type":"agent.message", "content":[{"text":"partial"}]}),
                json!({"type":"span.model_request_end", "usage":{"input_tokens":1, "output_tokens":1}}),
                json!({"type":"user.interrupt"}),
                json!({"type":"session.status_idle"}),
            ],
            expectation(),
        )
        .unwrap();
        assert!(codex.interrupted);
        assert!(claude.interrupted);
        assert_eq!(codex.terminal_state, "interrupted");
        assert_eq!(claude.terminal_state, "interrupted");
    }

    #[test]
    fn lifecycle_order_and_single_terminal_state_are_fail_closed() {
        let terminal_before_start = validate_trace(
            StructuredAdapterKind::CodexAppServer,
            &[
                ready("codex-thread-order"),
                invoked(),
                json!({"method":"turn/completed", "params":{"turn":{"status":"completed"}}}),
                json!({"method":"turn/started", "params":{}}),
                json!({"method":"thread/tokenUsage/updated", "params":{"tokenUsage":{"total":{"inputTokens":1, "outputTokens":1}}}}),
                result("out of order"),
            ],
            expectation(),
        )
        .unwrap_err();
        assert!(terminal_before_start
            .to_string()
            .contains("terminal state preceded"));

        let duplicate_terminal = validate_trace(
            StructuredAdapterKind::ClaudeAgentSdk,
            &[
                ready("claude-session-terminal"),
                invoked(),
                json!({"type":"session.status_running"}),
                json!({"type":"span.model_request_end", "usage":{"input_tokens":1, "output_tokens":1}}),
                json!({"type":"session.status_idle"}),
                json!({"type":"session.status_idle"}),
                result("duplicate terminal"),
            ],
            expectation(),
        )
        .unwrap_err();
        assert!(duplicate_terminal
            .to_string()
            .contains("exactly one lifecycle start and terminal"));
    }

    #[test]
    fn claude_usage_accumulates_across_model_requests() {
        let report = validate_trace(
            StructuredAdapterKind::ClaudeAgentSdk,
            &[
                ready("claude-session-usage"),
                invoked(),
                json!({"type":"session.status_running"}),
                json!({"type":"span.model_request_end", "usage":{"input_tokens":3, "output_tokens":2}}),
                json!({"type":"span.model_request_end", "usage":{"input_tokens":5, "output_tokens":7}}),
                json!({"type":"session.status_idle"}),
                result("usage accumulated"),
            ],
            expectation(),
        )
        .unwrap();
        assert_eq!(report.input_tokens, 8);
        assert_eq!(report.output_tokens, 9);
    }

    #[test]
    fn missing_invocation_is_typed_for_skill_aware_rerouting() {
        let error = validate_trace(
            StructuredAdapterKind::CodexAppServer,
            &[ready("codex-thread-skill-failure")],
            expectation(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            TyrionError::RequiredSkillUnavailable {
                configuration_id,
                skill_name,
                content_digest,
                ..
            } if configuration_id == "codex-deep"
                && skill_name == "code-review"
                && content_digest == format!("sha256:{}", "1".repeat(64))
        ));
    }
}

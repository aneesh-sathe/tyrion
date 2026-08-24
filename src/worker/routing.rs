use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::{AssignmentResources, WorkerRequirements};
use crate::TyrionError;

const REQUIRED_ADAPTER_CAPABILITIES: [&str; 7] = [
    "structured_lifecycle",
    "semantic_interrupt",
    "terminal_state",
    "usage",
    "skills",
    "result_submission",
    "contained",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerCatalog {
    configurations: Vec<WorkerConfiguration>,
}

#[derive(Clone)]
pub(super) struct ContainedCodexDescriptor {
    pub id: String,
    pub version: String,
    pub model: String,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub max_storage_bytes: u64,
    pub containment_profile: String,
    pub supports_claude: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerConfiguration {
    pub id: String,
    pub harness: String,
    pub adapter: WorkerAdapter,
    pub model: String,
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub context: WorkerContext,
    pub resource_limits: WorkerResourceLimits,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub authority_actions: Vec<String>,
    #[serde(default)]
    pub authority_scope_types: Vec<String>,
    #[serde(default)]
    pub assignment_constraints: Vec<String>,
    pub containment_profile: String,
    pub replacement_class: String,
    pub available: bool,
    pub metrics: WorkerMetrics,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerAdapter {
    pub kind: WorkerAdapterKind,
    pub version: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum WorkerAdapterKind {
    CodexAppServer,
    ClaudeAgentSdk,
    DeterministicLocal,
    ContainedCodex,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerContext {
    pub strategy: String,
    pub capacity_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerResourceLimits {
    pub max_concurrency_slots: u32,
    pub max_storage_bytes: u64,
    pub max_model_spend_cents: u64,
    pub max_paid_service_spend_cents: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerMetrics {
    pub expected_verified_correctness: u16,
    pub preference_adherence: u16,
    pub first_pass_acceptance: u16,
    pub commission_elapsed_time_contribution_ms: u64,
    pub cost_cents: u64,
    pub continuity: u16,
}

#[derive(Clone, Debug)]
pub(super) struct RouteRequest<'a> {
    pub requirements: &'a WorkerRequirements,
    pub resources: &'a AssignmentResources,
    pub required_authority_action: &'a str,
    pub required_authority_scope_types: &'a [&'a str],
    pub entry_harness: &'a str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RouteDecision {
    pub status: RouteStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_configuration: Option<WorkerConfiguration>,
    pub rationale: RoutingRationale,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RouteStatus {
    Selected,
    AttentionRequired,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RoutingRationale {
    pub entry_harness: String,
    pub entry_harness_preference_applied: bool,
    pub ordering: [&'static str; 6],
    pub eligible: Vec<String>,
    pub ineligible: Vec<IneligibleConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_unavailable_configuration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_replacement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention_requirement: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct IneligibleConfiguration {
    pub configuration_id: String,
    pub failed_gates: Vec<&'static str>,
}

impl WorkerCatalog {
    pub(super) fn load(
        path: Option<&Path>,
        contained_codex: Option<ContainedCodexDescriptor>,
    ) -> Result<Self, TyrionError> {
        let mut catalog = match path {
            Some(path) => serde_json::from_slice::<Self>(&fs::read(path)?)?,
            None => Self {
                configurations: vec![contained_codex
                    .clone()
                    .map(contained_codex_configuration)
                    .unwrap_or_else(deterministic_configuration)],
            },
        };
        catalog.validate()?;
        if let Some(descriptor) = contained_codex {
            catalog.bind_structured_containment(&descriptor)?;
        }
        Ok(catalog)
    }

    fn bind_structured_containment(
        &mut self,
        descriptor: &ContainedCodexDescriptor,
    ) -> Result<(), TyrionError> {
        for configuration in &mut self.configurations {
            if !matches!(
                configuration.adapter.kind,
                WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ClaudeAgentSdk
            ) {
                continue;
            }
            if configuration.available
                && configuration.adapter.kind == WorkerAdapterKind::ClaudeAgentSdk
                && !descriptor.supports_claude
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "available Worker Configuration {} requires a pinned Claude OpenShell profile",
                    configuration.id
                )));
            }
            if configuration.containment_profile != "openshell-repaired-v0.0.104"
                && configuration.containment_profile != descriptor.containment_profile
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} requires containment profile {}, which the configured runtime does not provide",
                    configuration.id, configuration.containment_profile
                )));
            }
            configuration.containment_profile = descriptor.containment_profile.clone();
        }
        Ok(())
    }

    pub(super) fn route(
        &self,
        request: RouteRequest<'_>,
        unavailable_configuration_ids: &HashSet<String>,
    ) -> RouteDecision {
        let mut eligible = Vec::new();
        let mut ineligible = Vec::new();
        for configuration in &self.configurations {
            let failed_gates = failed_gates(configuration, &request);
            if failed_gates.is_empty() {
                eligible.push(configuration);
            } else {
                ineligible.push(IneligibleConfiguration {
                    configuration_id: configuration.id.clone(),
                    failed_gates,
                });
            }
        }
        eligible.sort_by(|left, right| compare_configurations(left, right));
        let eligible_ids = eligible
            .iter()
            .map(|configuration| configuration.id.clone())
            .collect::<Vec<_>>();
        let preferred = eligible.first().copied();
        let (selected, preferred_unavailable_configuration, automatic_replacement) = match preferred
        {
            Some(configuration)
                if configuration_is_available(configuration)
                    && !unavailable_configuration_ids.contains(&configuration.id) =>
            {
                (Some(configuration), None, None)
            }
            Some(configuration) => {
                let replacement = eligible.iter().copied().skip(1).find(|candidate| {
                    configuration_is_available(candidate)
                        && !unavailable_configuration_ids.contains(&candidate.id)
                        && approximately_equal(configuration, candidate)
                });
                (
                    replacement,
                    Some(configuration.id.clone()),
                    replacement.map(|candidate| candidate.id.clone()),
                )
            }
            None => (None, None, None),
        };
        let attention_requirement = selected.is_none().then(|| match preferred {
            Some(configuration) => format!(
                "Make Worker Configuration {} available or approve an eligible approximately equal replacement.",
                configuration.id
            ),
            None => "Provide a Worker Configuration that passes every capability, authority, tool, Skill, context, Assignment, and resource gate.".into(),
        });
        RouteDecision {
            status: if selected.is_some() {
                RouteStatus::Selected
            } else {
                RouteStatus::AttentionRequired
            },
            selected_configuration: selected.cloned(),
            rationale: RoutingRationale {
                entry_harness: request.entry_harness.to_owned(),
                entry_harness_preference_applied: false,
                ordering: [
                    "expected_verified_correctness",
                    "preference_adherence",
                    "first_pass_acceptance",
                    "commission_elapsed_time_contribution",
                    "cost",
                    "continuity",
                ],
                eligible: eligible_ids,
                ineligible,
                preferred_unavailable_configuration,
                automatic_replacement,
                attention_requirement,
            },
        }
    }

    pub(super) fn requires_structured_runtime(&self) -> bool {
        self.configurations.iter().any(|configuration| {
            configuration.available
                && matches!(
                    configuration.adapter.kind,
                    WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ClaudeAgentSdk
                )
        })
    }

    pub(super) fn materialize_structured_adapters(
        &mut self,
        data_dir: &Path,
    ) -> Result<(), TyrionError> {
        let adapter_dir = data_dir.join("worker-adapters");
        fs::create_dir_all(&adapter_dir)?;
        fs::set_permissions(&adapter_dir, fs::Permissions::from_mode(0o700))?;
        for configuration in &mut self.configurations {
            if !configuration.available
                || !matches!(
                    configuration.adapter.kind,
                    WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ClaudeAgentSdk
                )
            {
                continue;
            }
            let source = Path::new(&configuration.adapter.command[0]);
            let destination = adapter_dir.join(&configuration.adapter.sha256);
            if !destination.exists() {
                fs::copy(source, &destination)?;
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))?;
            }
            let actual = format!("{:x}", Sha256::digest(fs::read(&destination)?));
            if actual != configuration.adapter.sha256.to_ascii_lowercase() {
                return Err(TyrionError::InvalidRequest(format!(
                    "cached Worker adapter {} failed its pinned digest",
                    configuration.id
                )));
            }
            configuration.adapter.command[0] = destination.to_string_lossy().into_owned();
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), TyrionError> {
        if self.configurations.is_empty() {
            return Err(TyrionError::InvalidRequest(
                "Worker catalog must contain at least one complete Worker Configuration".into(),
            ));
        }
        let mut ids = HashSet::new();
        for configuration in &self.configurations {
            if configuration.id.trim().is_empty()
                || configuration.harness.trim().is_empty()
                || configuration.adapter.version.trim().is_empty()
                || configuration.model.trim().is_empty()
                || configuration.context.strategy.trim().is_empty()
                || configuration.containment_profile.trim().is_empty()
                || configuration.replacement_class.trim().is_empty()
            {
                return Err(TyrionError::InvalidRequest(
                    "Worker Configuration identity, adapter, model, context, containment, and replacement class must not be empty".into(),
                ));
            }
            if !ids.insert(configuration.id.as_str()) {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration id {} is duplicated",
                    configuration.id
                )));
            }
            let expected_harness = match configuration.adapter.kind {
                WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ContainedCodex => "codex",
                WorkerAdapterKind::ClaudeAgentSdk => "claude",
                WorkerAdapterKind::DeterministicLocal => "tyrion",
            };
            if configuration.harness != expected_harness {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} adapter kind requires harness {}",
                    configuration.id, expected_harness
                )));
            }
            let named_values = [
                ("tool", &configuration.tools),
                ("Skill", &configuration.skills),
                ("capability", &configuration.capabilities),
                ("authority action", &configuration.authority_actions),
                ("authority scope type", &configuration.authority_scope_types),
                (
                    "Assignment constraint",
                    &configuration.assignment_constraints,
                ),
            ];
            for (name, values) in named_values {
                let mut unique = HashSet::new();
                for value in values {
                    if value.trim().is_empty() || value.contains('\0') {
                        return Err(TyrionError::InvalidRequest(format!(
                            "Worker Configuration {} {name} names must not be empty",
                            configuration.id
                        )));
                    }
                    if !unique.insert(value) {
                        return Err(TyrionError::InvalidRequest(format!(
                            "Worker Configuration {} {name} {} is duplicated",
                            configuration.id, value
                        )));
                    }
                }
            }
            if configuration
                .authority_scope_types
                .iter()
                .any(|scope_type| {
                    !matches!(
                        scope_type.as_str(),
                        "repository" | "path" | "action" | "destination" | "effect"
                    )
                })
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} has an unknown authority scope type",
                    configuration.id
                )));
            }
            if configuration.resource_limits.max_concurrency_slots == 0
                || configuration.resource_limits.max_storage_bytes == 0
                || configuration.context.capacity_tokens == 0
            {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} requires positive concurrency, storage, and context limits",
                    configuration.id
                )));
            }
            let supported_context_strategy = match configuration.adapter.kind {
                WorkerAdapterKind::DeterministicLocal => {
                    configuration.context.strategy == "exact_assignment"
                }
                WorkerAdapterKind::CodexAppServer
                | WorkerAdapterKind::ClaudeAgentSdk
                | WorkerAdapterKind::ContainedCodex => matches!(
                    configuration.context.strategy.as_str(),
                    "fresh" | "fresh_with_retrieval"
                ),
            };
            if !supported_context_strategy {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} has unsupported context strategy {}",
                    configuration.id, configuration.context.strategy
                )));
            }
            let metric_scores = [
                configuration.metrics.expected_verified_correctness,
                configuration.metrics.preference_adherence,
                configuration.metrics.first_pass_acceptance,
                configuration.metrics.continuity,
            ];
            if metric_scores.iter().any(|score| *score > 10_000) {
                return Err(TyrionError::InvalidRequest(format!(
                    "Worker Configuration {} scores must be basis points between 0 and 10000",
                    configuration.id
                )));
            }
            if matches!(
                configuration.adapter.kind,
                WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ClaudeAgentSdk
            ) {
                if !configuration
                    .authority_scope_types
                    .iter()
                    .any(|scope_type| scope_type == "action")
                {
                    return Err(TyrionError::InvalidRequest(format!(
                        "Worker Configuration {} must declare action authority compatibility",
                        configuration.id
                    )));
                }
                if configuration
                    .authority_scope_types
                    .iter()
                    .any(|scope_type| matches!(scope_type.as_str(), "destination" | "effect"))
                {
                    return Err(TyrionError::InvalidRequest(format!(
                        "Worker Configuration {} cannot claim external destination or effect authority under the current structured contract",
                        configuration.id
                    )));
                }
                if configuration.available
                    && (configuration.adapter.command.is_empty()
                        || !Path::new(&configuration.adapter.command[0]).is_absolute()
                        || configuration
                            .adapter
                            .command
                            .iter()
                            .any(|argument| argument.is_empty() || argument.contains('\0')))
                {
                    return Err(TyrionError::InvalidRequest(format!(
                        "available Worker Configuration {} requires an absolute structured adapter command with safe argv",
                        configuration.id
                    )));
                }
                if configuration.available {
                    let executable = Path::new(&configuration.adapter.command[0]);
                    let metadata = fs::metadata(executable).map_err(|error| {
                        TyrionError::InvalidRequest(format!(
                            "available Worker Configuration {} adapter executable {} is unavailable: {error}",
                            configuration.id,
                            executable.display()
                        ))
                    })?;
                    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                        return Err(TyrionError::InvalidRequest(format!(
                            "available Worker Configuration {} adapter executable {} is not an executable file",
                            configuration.id,
                            executable.display()
                        )));
                    }
                    if configuration.adapter.sha256.len() != 64
                        || !configuration
                            .adapter
                            .sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                    {
                        return Err(TyrionError::InvalidRequest(format!(
                            "available Worker Configuration {} requires a SHA-256 adapter digest",
                            configuration.id
                        )));
                    }
                    let actual = format!("{:x}", Sha256::digest(fs::read(executable)?));
                    if actual != configuration.adapter.sha256.to_ascii_lowercase() {
                        return Err(TyrionError::InvalidRequest(format!(
                            "available Worker Configuration {} adapter digest does not match {}",
                            configuration.id,
                            executable.display()
                        )));
                    }
                }
                for capability in REQUIRED_ADAPTER_CAPABILITIES {
                    if !configuration
                        .capabilities
                        .iter()
                        .any(|candidate| candidate == capability)
                    {
                        return Err(TyrionError::InvalidRequest(format!(
                            "Worker Configuration {} adapter contract is missing capability {}",
                            configuration.id, capability
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn configuration_is_available(configuration: &WorkerConfiguration) -> bool {
    if !configuration.available {
        return false;
    }
    if !matches!(
        configuration.adapter.kind,
        WorkerAdapterKind::CodexAppServer | WorkerAdapterKind::ClaudeAgentSdk
    ) {
        return true;
    }
    let Some(executable) = configuration.adapter.command.first().map(Path::new) else {
        return false;
    };
    fs::read(executable).is_ok_and(|content| {
        format!("{:x}", Sha256::digest(content))
            == configuration.adapter.sha256.to_ascii_lowercase()
    })
}

fn failed_gates(
    configuration: &WorkerConfiguration,
    request: &RouteRequest<'_>,
) -> Vec<&'static str> {
    let requirements = request.requirements;
    let mut failures = Vec::new();
    if !requirements.require_configurations.is_empty()
        && !requirements
            .require_configurations
            .iter()
            .any(|id| id == &configuration.id)
    {
        failures.push("required_configuration");
    }
    if requirements
        .exclude_configurations
        .iter()
        .any(|id| id == &configuration.id)
    {
        failures.push("excluded_configuration");
    }
    if !contains_all(&configuration.capabilities, &requirements.capabilities) {
        failures.push("required_capabilities");
    }
    if !contains_all(&configuration.tools, &requirements.tools) {
        failures.push("required_tools");
    }
    if !contains_all(&configuration.skills, &requirements.skills) {
        failures.push("required_skills");
    }
    if configuration.context.capacity_tokens < requirements.min_context_tokens {
        failures.push("context_capacity");
    }
    if requirements
        .context_strategy
        .as_deref()
        .is_some_and(|required| required != configuration.context.strategy)
    {
        failures.push("context_strategy");
    }
    if !contains_all(
        &configuration.assignment_constraints,
        &requirements.assignment_constraints,
    ) {
        failures.push("assignment_constraints");
    }
    if !configuration
        .authority_actions
        .iter()
        .any(|action| action == request.required_authority_action)
    {
        failures.push("authority_compatibility");
    }
    if request
        .required_authority_scope_types
        .iter()
        .any(|required| {
            !configuration
                .authority_scope_types
                .iter()
                .any(|actual| actual == *required)
        })
    {
        failures.push("authority_scope_compatibility");
    }
    let limits = &configuration.resource_limits;
    let resources = request.resources;
    if resources.concurrency_slots > limits.max_concurrency_slots
        || resources.max_storage_bytes > limits.max_storage_bytes
        || resources.max_model_spend_cents > limits.max_model_spend_cents
        || resources.max_paid_service_spend_cents > limits.max_paid_service_spend_cents
    {
        failures.push("resource_limits");
    }
    failures
}

fn contains_all(actual: &[String], required: &[String]) -> bool {
    required
        .iter()
        .all(|required| actual.iter().any(|candidate| candidate == required))
}

fn compare_configurations(left: &WorkerConfiguration, right: &WorkerConfiguration) -> Ordering {
    right
        .metrics
        .expected_verified_correctness
        .cmp(&left.metrics.expected_verified_correctness)
        .then_with(|| {
            right
                .metrics
                .preference_adherence
                .cmp(&left.metrics.preference_adherence)
        })
        .then_with(|| {
            right
                .metrics
                .first_pass_acceptance
                .cmp(&left.metrics.first_pass_acceptance)
        })
        .then_with(|| {
            left.metrics
                .commission_elapsed_time_contribution_ms
                .cmp(&right.metrics.commission_elapsed_time_contribution_ms)
        })
        .then_with(|| left.metrics.cost_cents.cmp(&right.metrics.cost_cents))
        .then_with(|| right.metrics.continuity.cmp(&left.metrics.continuity))
        .then_with(|| left.id.cmp(&right.id))
}

fn approximately_equal(preferred: &WorkerConfiguration, candidate: &WorkerConfiguration) -> bool {
    preferred.replacement_class == candidate.replacement_class
        && preferred
            .metrics
            .expected_verified_correctness
            .saturating_sub(candidate.metrics.expected_verified_correctness)
            <= 100
        && preferred
            .metrics
            .preference_adherence
            .saturating_sub(candidate.metrics.preference_adherence)
            <= 100
        && preferred
            .metrics
            .first_pass_acceptance
            .saturating_sub(candidate.metrics.first_pass_acceptance)
            <= 100
}

fn deterministic_configuration() -> WorkerConfiguration {
    WorkerConfiguration {
        id: "deterministic-local-v1".into(),
        harness: "tyrion".into(),
        adapter: WorkerAdapter {
            kind: WorkerAdapterKind::DeterministicLocal,
            version: "1".into(),
            sha256: String::new(),
            command: Vec::new(),
        },
        model: "deterministic-echo".into(),
        settings: BTreeMap::new(),
        tools: Vec::new(),
        skills: Vec::new(),
        context: WorkerContext {
            strategy: "exact_assignment".into(),
            capacity_tokens: u64::MAX,
        },
        resource_limits: WorkerResourceLimits {
            max_concurrency_slots: u32::MAX,
            max_storage_bytes: u64::MAX,
            max_model_spend_cents: u64::MAX,
            max_paid_service_spend_cents: u64::MAX,
        },
        capabilities: vec!["structured_lifecycle".into(), "terminal_state".into()],
        authority_actions: vec![super::DETERMINISTIC_ACTION.into()],
        authority_scope_types: vec!["action".into()],
        assignment_constraints: Vec::new(),
        containment_profile: "in-process-deterministic".into(),
        replacement_class: "deterministic".into(),
        available: true,
        metrics: WorkerMetrics {
            expected_verified_correctness: 10_000,
            preference_adherence: 10_000,
            first_pass_acceptance: 10_000,
            commission_elapsed_time_contribution_ms: 0,
            cost_cents: 0,
            continuity: 0,
        },
    }
}

fn contained_codex_configuration(descriptor: ContainedCodexDescriptor) -> WorkerConfiguration {
    let mut configuration = deterministic_configuration();
    configuration.id = descriptor.id;
    configuration.harness = "codex".into();
    configuration.adapter.kind = WorkerAdapterKind::ContainedCodex;
    configuration.adapter.version = descriptor.version;
    configuration.model = descriptor.model;
    configuration.settings = descriptor.settings;
    configuration.tools = vec!["git".into()];
    configuration.context.strategy = "fresh".into();
    configuration.capabilities = vec![
        "structured_lifecycle".into(),
        "terminal_state".into(),
        "result_submission".into(),
        "contained".into(),
    ];
    configuration.authority_actions = vec![super::CODEX_GIT_ACTION.into()];
    configuration.authority_scope_types = vec!["repository".into(), "path".into(), "action".into()];
    configuration.assignment_constraints = vec!["coding".into()];
    configuration.resource_limits.max_storage_bytes = descriptor.max_storage_bytes;
    configuration.containment_profile = descriptor.containment_profile;
    configuration.replacement_class = "contained-coding".into();
    configuration
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (WorkerConfiguration, WorkerConfiguration) {
        let mut left = deterministic_configuration();
        left.id = "left".into();
        left.metrics = WorkerMetrics {
            expected_verified_correctness: 5000,
            preference_adherence: 5000,
            first_pass_acceptance: 5000,
            commission_elapsed_time_contribution_ms: 5000,
            cost_cents: 5000,
            continuity: 5000,
        };
        let mut right = left.clone();
        right.id = "right".into();
        (left, right)
    }

    fn left_ranks_first(mutate: impl FnOnce(&mut WorkerConfiguration)) {
        let (mut left, right) = pair();
        mutate(&mut left);
        assert_eq!(compare_configurations(&left, &right), Ordering::Less);
    }

    #[test]
    fn eligible_configuration_order_is_declared_lexicographic_order() {
        left_ranks_first(|left| left.metrics.expected_verified_correctness += 1);
        left_ranks_first(|left| left.metrics.preference_adherence += 1);
        left_ranks_first(|left| left.metrics.first_pass_acceptance += 1);
        left_ranks_first(|left| left.metrics.commission_elapsed_time_contribution_ms -= 1);
        left_ranks_first(|left| left.metrics.cost_cents -= 1);
        left_ranks_first(|left| left.metrics.continuity += 1);
    }

    #[test]
    fn catalog_rejects_an_unsupported_context_strategy() {
        let mut configuration = deterministic_configuration();
        configuration.context.strategy = "resume".into();
        let catalog = WorkerCatalog {
            configurations: vec![configuration],
        };

        assert!(catalog
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported context strategy"));
    }
}

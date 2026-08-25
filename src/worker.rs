use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::artifact::ArtifactRevision;
use crate::domain::EvidenceOutcome;
use crate::protocol::{
    AssignmentResources, AuthorityEnvelope, ExecutionSpec, SkillVersion, VerificationDefect,
    VerificationDepth, Verifier, VerifierType, WorkerRequirements,
};
use crate::TyrionError;

mod contained_codex;
mod routing;
mod structured_process;

pub const DETERMINISTIC_ACTION: &str = "deterministic.echo";
pub const CODEX_GIT_ACTION: &str = "codex.git_change";

pub(crate) struct WorkerRuntime {
    contained_codex: Option<contained_codex::ContainedCodexRuntime>,
    catalog: routing::WorkerCatalog,
    corrupt_artifact_revision: bool,
    incorrect_first_result: bool,
    incorrect_result_commissions: Mutex<std::collections::HashSet<String>>,
    controls: Mutex<std::collections::HashMap<String, Arc<WorkerControl>>>,
    integration_locks: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    hold_for_control: bool,
    hold_before_integration: bool,
    hold_after_integration: bool,
    hold_after_external_integration: bool,
}

pub(crate) struct WorkerRuntimeOptions {
    pub(crate) corrupt_artifact_revision: bool,
    pub(crate) incorrect_first_result: bool,
    pub(crate) hold_for_control: bool,
    pub(crate) hold_before_integration: bool,
    pub(crate) hold_after_integration: bool,
    pub(crate) hold_after_external_integration: bool,
}

pub(crate) struct AttemptControlScope<'a> {
    runtime: &'a WorkerRuntime,
    attempt_id: String,
}

impl Drop for AttemptControlScope<'_> {
    fn drop(&mut self) {
        let _ = self.runtime.end_attempt(&self.attempt_id);
    }
}

pub(super) struct WorkerControl {
    mandate_revision: i64,
    interrupted: AtomicBool,
    interrupt_reason: Mutex<Option<(String, String)>>,
    watchdog_signal: Mutex<Option<&'static str>>,
    clarifications: Mutex<Vec<(String, String)>>,
    delivered_commands: Mutex<std::collections::HashSet<String>>,
    changed: Condvar,
    adapter_input: Mutex<Option<ChildStdin>>,
    adapter_was_attached: AtomicBool,
    adapter_detached: AtomicBool,
    telemetry: Mutex<LiveWorkerTelemetry>,
}

#[derive(Clone, Default)]
struct LiveWorkerTelemetry {
    native_session_id: Option<String>,
    latest_activity: Option<String>,
    activity_at_ms: Option<i64>,
    input_tokens: u64,
    output_tokens: u64,
    usage_reported: bool,
    skill_versions: Vec<SkillVersion>,
}

impl WorkerControl {
    pub(super) fn attach_adapter_input(&self, mut input: ChildStdin) -> Result<(), TyrionError> {
        let mut adapter_input = self.adapter_input.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker adapter control channel is unavailable".into())
        })?;
        for (command_id, clarification) in self
            .clarifications
            .lock()
            .map_err(|_| {
                TyrionError::InvalidRequest("Worker clarification channel is unavailable".into())
            })?
            .iter()
        {
            write_adapter_control(
                &mut input,
                &serde_json::json!({
                    "type": "tyrion.worker.steer",
                    "command_id": command_id,
                    "scope": "assignment_clarification_only",
                    "mandate_revision": self.mandate_revision,
                    "immutable": ["goal", "authority", "criteria", "resource_ceilings"],
                    "clarification": clarification,
                }),
            )?;
        }
        if self.interrupted.load(Ordering::SeqCst) {
            let (command_id, reason) = self
                .interrupt_reason
                .lock()
                .map_err(|_| {
                    TyrionError::InvalidRequest("Worker interruption channel is unavailable".into())
                })?
                .clone()
                .ok_or_else(|| {
                    TyrionError::InvalidRequest("Worker interruption reason is unavailable".into())
                })?;
            write_adapter_control(
                &mut input,
                &serde_json::json!({
                    "type": "tyrion.worker.interrupt",
                    "command_id": command_id,
                    "mandate_revision": self.mandate_revision,
                    "reason": reason,
                }),
            )?;
        }
        *adapter_input = Some(input);
        self.adapter_was_attached.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub(super) fn detach_adapter_input(&self) -> Result<(), TyrionError> {
        let mut adapter_input = self.adapter_input.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker adapter control channel is unavailable".into())
        })?;
        if self.adapter_was_attached.load(Ordering::SeqCst) {
            self.adapter_detached.store(true, Ordering::SeqCst);
        }
        adapter_input.take();
        Ok(())
    }

    pub(super) fn was_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub(in crate::worker) fn observe_adapter_event(
        &self,
        kind: routing::WorkerAdapterKind,
        event: &serde_json::Value,
    ) -> Result<(), TyrionError> {
        let mut telemetry = self.telemetry.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker telemetry channel is unavailable".into())
        })?;
        let mut meaningful_activity = None;
        if event["type"] == "tyrion.adapter.ready" {
            telemetry.native_session_id = event["native_session_id"].as_str().map(str::to_owned);
            meaningful_activity = Some("Structured adapter ready");
        }
        match kind {
            routing::WorkerAdapterKind::CodexAppServer => match event["method"].as_str() {
                Some("turn/started") => meaningful_activity = Some("Codex turn started"),
                Some("item/completed") => {
                    meaningful_activity = Some("Codex produced a structured Result")
                }
                Some("thread/tokenUsage/updated") => {
                    let total = &event["params"]["tokenUsage"]["total"];
                    if let (Some(input), Some(output)) = (
                        total["inputTokens"].as_u64(),
                        total["outputTokens"].as_u64(),
                    ) {
                        telemetry.input_tokens = input;
                        telemetry.output_tokens = output;
                        telemetry.usage_reported = true;
                    }
                }
                Some("error") => meaningful_activity = Some("Codex adapter reported an error"),
                _ => {}
            },
            routing::WorkerAdapterKind::ClaudeAgentSdk => match event["type"].as_str() {
                Some("session.status_running") => {
                    meaningful_activity = Some("Claude session started")
                }
                Some("agent.message") => {
                    meaningful_activity = Some("Claude produced a structured Result")
                }
                Some("span.model_request_end") => {
                    if let (Some(input), Some(output)) = (
                        event["usage"]["input_tokens"].as_u64(),
                        event["usage"]["output_tokens"].as_u64(),
                    ) {
                        telemetry.input_tokens = telemetry.input_tokens.saturating_add(input);
                        telemetry.output_tokens = telemetry.output_tokens.saturating_add(output);
                        telemetry.usage_reported = true;
                    }
                }
                Some("session.error") => {
                    meaningful_activity = Some("Claude adapter reported an error")
                }
                _ => {}
            },
            _ => {}
        }
        if let Some(activity) = meaningful_activity {
            telemetry.latest_activity = Some(activity.into());
            telemetry.activity_at_ms = Some(current_time_millis()?);
        }
        Ok(())
    }

    pub(in crate::worker) fn record_validated_skill_versions(
        &self,
        skill_versions: &[SkillVersion],
    ) -> Result<(), TyrionError> {
        self.telemetry
            .lock()
            .map_err(|_| {
                TyrionError::InvalidRequest("Worker telemetry channel is unavailable".into())
            })?
            .skill_versions = skill_versions.to_vec();
        Ok(())
    }
}

fn write_adapter_control(
    input: &mut ChildStdin,
    value: &serde_json::Value,
) -> Result<(), TyrionError> {
    serde_json::to_writer(&mut *input, value)?;
    input.write_all(b"\n")?;
    input.flush()?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct AssignmentContext {
    pub commission_id: String,
    pub assignment_id: String,
    pub attempt_id: String,
    pub mandate_revision: i64,
    pub plan_revision: i64,
    pub goal: String,
    pub execution: ExecutionSpec,
    pub selected_configuration: serde_json::Value,
    pub worker_context_packet: serde_json::Value,
    pub skill_defaults: Vec<AssignmentSkillDefault>,
    pub criteria: Vec<CriterionDefinition>,
    pub authority: AuthorityEnvelope,
    pub authorized_paths: Vec<String>,
    pub declared_write_scopes: Vec<String>,
    pub comparison_candidates: Vec<ComparisonCandidate>,
    pub max_storage_bytes: u64,
    pub max_model_spend_cents: u64,
    pub max_paid_service_spend_cents: u64,
    pub lease_expires_at: i64,
}

#[derive(Clone, Serialize)]
pub(crate) struct AssignmentSkillDefault {
    #[serde(flatten)]
    pub version: crate::protocol::SkillVersion,
    pub requirement: AssignmentSkillRequirement,
    pub provenance: crate::protocol::SkillSelectionProvenance,
    pub delegation: NativeSkillDelegation,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssignmentSkillRequirement {
    Required,
    Selected,
}

impl AssignmentSkillRequirement {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Selected => "selected",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "required" => Some(Self::Required),
            "selected" => Some(Self::Selected),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeSkillDelegation {
    NativeUnchanged,
}

impl NativeSkillDelegation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NativeUnchanged => "native_unchanged",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "native_unchanged" => Some(Self::NativeUnchanged),
            _ => None,
        }
    }
}

impl std::ops::Deref for AssignmentSkillDefault {
    type Target = crate::protocol::SkillVersion;

    fn deref(&self) -> &Self::Target {
        &self.version
    }
}

#[derive(Clone)]
pub(crate) struct ComparisonCandidate {
    pub result_id: String,
    pub artifact_revision: String,
    pub summary: String,
    pub changed_paths: Vec<String>,
    pub verification_outcomes: serde_json::Value,
    pub bundle_path: PathBuf,
}

#[derive(Clone, Serialize)]
pub(crate) struct CriterionDefinition {
    pub id: String,
    pub required_evidence: String,
    pub verifier_type: VerifierType,
    pub verification_depth: VerificationDepth,
    pub verifier_configuration: String,
    pub verification_environment: String,
    pub verifier: Verifier,
}

pub(crate) struct CandidateResult {
    pub output: String,
    pub artifact_revision: ArtifactRevision,
    pub base_revision: Option<String>,
    pub candidate_commits: Vec<String>,
    pub changed_paths: Vec<String>,
    pub artifacts: Vec<ArtifactRecord>,
    pub known_effects: Vec<String>,
    pub native_session_id: Option<String>,
    pub usage: serde_json::Value,
    pub latest_meaningful_activity: String,
    state: CandidateState,
}

enum CandidateState {
    Deterministic,
    CodexGit(contained_codex::GitCandidateState),
}

pub(crate) struct IntegratedResult {
    pub artifact_revision: ArtifactRevision,
    pub artifacts: Vec<ArtifactRecord>,
    state: IntegratedState,
}

enum IntegratedState {
    Deterministic(String),
    CodexGit(contained_codex::GitIntegratedState),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ArtifactRecord {
    pub kind: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct VerificationRecord {
    pub criterion_id: String,
    pub evidence_type: String,
    pub verifier_type: VerifierType,
    pub verification_attempt_id: String,
    pub verifier_identity: String,
    pub verifier_configuration: String,
    pub verifier_kind: VerificationKind,
    pub procedure: Verifier,
    pub environment: String,
    pub scope: VerificationScope,
    pub outcome: EvidenceOutcome,
    pub observed: String,
    pub expected: String,
    pub material_contradiction: bool,
    pub defect: Option<VerificationDefect>,
    pub producer_attempt_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationKind {
    ExactMatch,
    Command,
}

impl VerificationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ExactMatch => "exact_match",
            Self::Command => "command",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationScope {
    Candidate,
    Integrated,
}

impl VerificationScope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Integrated => "integrated",
        }
    }
}

impl VerificationRecord {
    pub(crate) fn passed(&self) -> bool {
        self.outcome == EvidenceOutcome::Passed
    }
}

impl WorkerRuntime {
    pub(crate) fn load(
        data_dir: &Path,
        codex_worker_config: Option<&Path>,
        worker_catalog: Option<&Path>,
        options: WorkerRuntimeOptions,
    ) -> Result<Self, TyrionError> {
        let contained_codex = codex_worker_config
            .map(|path| contained_codex::ContainedCodexRuntime::load(path, data_dir))
            .transpose()?;
        let mut catalog = routing::WorkerCatalog::load(
            worker_catalog,
            contained_codex
                .as_ref()
                .map(contained_codex::ContainedCodexRuntime::routing_descriptor),
        )?;
        if contained_codex.is_none() && catalog.requires_structured_runtime() {
            return Err(TyrionError::InvalidRequest(
                "available structured Worker Configurations require --codex-worker-config for Tyrion-owned containment"
                    .into(),
            ));
        }
        catalog.materialize_structured_adapters(data_dir)?;
        Ok(Self {
            contained_codex,
            catalog,
            corrupt_artifact_revision: options.corrupt_artifact_revision,
            incorrect_first_result: options.incorrect_first_result,
            incorrect_result_commissions: Mutex::new(std::collections::HashSet::new()),
            controls: Mutex::new(std::collections::HashMap::new()),
            integration_locks: Mutex::new(std::collections::HashMap::new()),
            hold_for_control: options.hold_for_control,
            hold_before_integration: options.hold_before_integration,
            hold_after_integration: options.hold_after_integration,
            hold_after_external_integration: options.hold_after_external_integration,
        })
    }

    pub(crate) fn begin_attempt(
        &self,
        attempt_id: &str,
        mandate_revision: i64,
    ) -> Result<(), TyrionError> {
        let mut controls = self.controls.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker control registry is unavailable".into())
        })?;
        controls.insert(
            attempt_id.to_owned(),
            Arc::new(WorkerControl {
                mandate_revision,
                interrupted: AtomicBool::new(false),
                interrupt_reason: Mutex::new(None),
                watchdog_signal: Mutex::new(None),
                clarifications: Mutex::new(Vec::new()),
                delivered_commands: Mutex::new(std::collections::HashSet::new()),
                changed: Condvar::new(),
                adapter_input: Mutex::new(None),
                adapter_was_attached: AtomicBool::new(false),
                adapter_detached: AtomicBool::new(false),
                telemetry: Mutex::new(LiveWorkerTelemetry::default()),
            }),
        );
        Ok(())
    }

    pub(crate) fn commission_integration_lock(
        &self,
        commission_id: &str,
    ) -> Result<Arc<Mutex<()>>, TyrionError> {
        let mut locks = self.integration_locks.lock().map_err(|_| {
            TyrionError::InvalidRequest(
                "Commission Integration lock registry is unavailable".into(),
            )
        })?;
        Ok(Arc::clone(
            locks
                .entry(commission_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    pub(crate) fn attempt_control_scope(&self, attempt_id: &str) -> AttemptControlScope<'_> {
        AttemptControlScope {
            runtime: self,
            attempt_id: attempt_id.to_owned(),
        }
    }

    pub(crate) fn steer(
        &self,
        attempt_id: &str,
        command_id: &str,
        clarification: &str,
    ) -> Result<(), TyrionError> {
        let control = self.control(attempt_id)?;
        let mut delivered = control.delivered_commands.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker command registry is unavailable".into())
        })?;
        if delivered.contains(command_id) {
            return Ok(());
        }
        let mut adapter_input = control.adapter_input.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker adapter control channel is unavailable".into())
        })?;
        if control.adapter_detached.load(Ordering::SeqCst) {
            return Err(TyrionError::ControlDenied(
                "the structured Worker has already reached a terminal boundary".into(),
            ));
        }
        if control.interrupted.load(Ordering::SeqCst) {
            return Err(TyrionError::ControlDenied(
                "the Worker has already been interrupted".into(),
            ));
        }
        control
            .clarifications
            .lock()
            .map_err(|_| {
                TyrionError::InvalidRequest("Worker clarification channel is unavailable".into())
            })?
            .push((command_id.to_owned(), clarification.to_owned()));
        if let Some(input) = adapter_input.as_mut() {
            write_adapter_control(
                input,
                &serde_json::json!({
                    "type": "tyrion.worker.steer",
                    "command_id": command_id,
                    "scope": "assignment_clarification_only",
                    "mandate_revision": control.mandate_revision,
                    "immutable": ["goal", "authority", "criteria", "resource_ceilings"],
                    "clarification": clarification,
                }),
            )?;
        }
        delivered.insert(command_id.to_owned());
        control.changed.notify_all();
        Ok(())
    }

    pub(crate) fn interrupt(
        &self,
        attempt_id: &str,
        command_id: &str,
        reason: &str,
    ) -> Result<(), TyrionError> {
        let control = self.control(attempt_id)?;
        let mut delivered = control.delivered_commands.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker command registry is unavailable".into())
        })?;
        if delivered.contains(command_id) {
            return Ok(());
        }
        let mut adapter_input = control.adapter_input.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker adapter control channel is unavailable".into())
        })?;
        if control.adapter_detached.load(Ordering::SeqCst) {
            return Err(TyrionError::ControlDenied(
                "the structured Worker has already reached a terminal boundary".into(),
            ));
        }
        if control.interrupted.load(Ordering::SeqCst) {
            return Err(TyrionError::ControlDenied(
                "the Worker has already been interrupted".into(),
            ));
        }
        let mut interrupt_reason = control.interrupt_reason.lock().map_err(|_| {
            TyrionError::InvalidRequest("Worker interruption channel is unavailable".into())
        })?;
        if let Some(input) = adapter_input.as_mut() {
            write_adapter_control(
                input,
                &serde_json::json!({
                    "type": "tyrion.worker.interrupt",
                    "command_id": command_id,
                    "mandate_revision": control.mandate_revision,
                    "reason": reason,
                }),
            )?;
        }
        *interrupt_reason = Some((command_id.to_owned(), reason.to_owned()));
        control.interrupted.store(true, Ordering::SeqCst);
        delivered.insert(command_id.to_owned());
        control.changed.notify_all();
        Ok(())
    }

    pub(crate) fn cancel_attempt(&self, attempt_id: &str) -> Result<(), TyrionError> {
        let command_id = format!("commission-cancel-{attempt_id}");
        self.interrupt(
            attempt_id,
            &command_id,
            "Commission cancelled by the Principal",
        )
    }

    pub(crate) fn watchdog_contain(
        &self,
        attempt_id: &str,
        signal: &'static str,
    ) -> Result<(), TyrionError> {
        let control = self.control(attempt_id)?;
        *control.watchdog_signal.lock().map_err(|_| {
            TyrionError::InvalidRequest("Watchdog signal registry is unavailable".into())
        })? = Some(signal);
        let command_id = format!("watchdog-{signal}-{attempt_id}");
        if control.adapter_detached.load(Ordering::SeqCst) {
            *control.interrupt_reason.lock().map_err(|_| {
                TyrionError::InvalidRequest("Worker interruption channel is unavailable".into())
            })? = Some((command_id, format!("Watchdog contained {signal}")));
            control.interrupted.store(true, Ordering::SeqCst);
            control.changed.notify_all();
            Ok(())
        } else {
            self.interrupt(
                attempt_id,
                &command_id,
                &format!("Watchdog contained {signal}"),
            )
        }
    }

    pub(crate) fn end_attempt(&self, attempt_id: &str) -> Result<(), TyrionError> {
        self.controls
            .lock()
            .map_err(|_| {
                TyrionError::InvalidRequest("Worker control registry is unavailable".into())
            })?
            .remove(attempt_id);
        Ok(())
    }

    pub(crate) fn cleanup_stranded_attempt(
        &self,
        attempt_id: &str,
        commission_id: &str,
        execution: &ExecutionSpec,
        artifact_revision: Option<&str>,
    ) -> Result<(), TyrionError> {
        let Some(runtime) = self.contained_codex.as_ref() else {
            return if matches!(execution, ExecutionSpec::Deterministic) {
                Ok(())
            } else {
                Err(TyrionError::InvalidRequest(
                    "codex_git containment cleanup requires the pinned contained runtime".into(),
                ))
            };
        };
        runtime.cleanup_stranded_attempt(attempt_id)?;
        if let ExecutionSpec::CodexGit { base_revision, .. } = execution {
            runtime.restore_integration_repository(
                commission_id,
                artifact_revision.unwrap_or(base_revision),
            )?;
        }
        Ok(())
    }

    pub(crate) fn is_attempt_active(&self, attempt_id: &str) -> bool {
        self.controls
            .lock()
            .is_ok_and(|controls| controls.contains_key(attempt_id))
    }

    pub(crate) fn accepts_live_control(&self, attempt_id: &str) -> bool {
        self.controls
            .lock()
            .ok()
            .and_then(|controls| controls.get(attempt_id).cloned())
            .is_some_and(|control| {
                !control.interrupted.load(Ordering::SeqCst)
                    && !control.adapter_detached.load(Ordering::SeqCst)
            })
    }

    pub(crate) fn live_telemetry(&self, attempt_id: &str) -> Option<serde_json::Value> {
        let control = self.controls.lock().ok()?.get(attempt_id).cloned()?;
        let telemetry = control.telemetry.lock().ok()?.clone();
        Some(serde_json::json!({
            "native_session_id": telemetry.native_session_id,
            "latest_meaningful_activity": telemetry.latest_activity,
            "activity_at_ms": telemetry.activity_at_ms,
            "usage": if telemetry.usage_reported {
                serde_json::json!({
                    "input_tokens": telemetry.input_tokens,
                    "output_tokens": telemetry.output_tokens,
                })
            } else {
                serde_json::json!({})
            },
            "skill_versions": telemetry.skill_versions,
        }))
    }

    fn control(&self, attempt_id: &str) -> Result<Arc<WorkerControl>, TyrionError> {
        self.controls
            .lock()
            .map_err(|_| {
                TyrionError::InvalidRequest("Worker control registry is unavailable".into())
            })?
            .get(attempt_id)
            .cloned()
            .ok_or_else(|| TyrionError::ControlDenied("the Worker is no longer active".into()))
    }

    pub(crate) fn route(
        &self,
        requirements: &WorkerRequirements,
        resources: &AssignmentResources,
        required_authority_action: &str,
        required_authority_scope_types: &[&str],
        entry_harness: &str,
        unavailable_configuration_ids: &std::collections::HashSet<String>,
    ) -> Result<serde_json::Value, TyrionError> {
        Ok(serde_json::to_value(self.catalog.route(
            routing::RouteRequest {
                requirements,
                resources,
                required_authority_action,
                required_authority_scope_types,
                entry_harness,
            },
            unavailable_configuration_ids,
        ))?)
    }

    pub(crate) fn assignment_execution(
        &self,
        proposed: &ExecutionSpec,
        commission_id: &str,
        current_artifact_revision: Option<&str>,
    ) -> ExecutionSpec {
        match (
            proposed,
            current_artifact_revision,
            self.contained_codex.as_ref(),
        ) {
            (ExecutionSpec::CodexGit { .. }, Some(base_revision), Some(contained_codex)) => {
                ExecutionSpec::CodexGit {
                    repository: contained_codex
                        .integration_repository(commission_id)
                        .to_string_lossy()
                        .into_owned(),
                    base_revision: base_revision.to_owned(),
                }
            }
            _ => proposed.clone(),
        }
    }

    pub(crate) fn lease_ttl_seconds(&self, execution: &ExecutionSpec) -> Result<u64, TyrionError> {
        match execution {
            ExecutionSpec::Deterministic => Ok(30),
            ExecutionSpec::CodexGit { .. } => self
                .contained_codex
                .as_ref()
                .map(contained_codex::ContainedCodexRuntime::lease_ttl_seconds)
                .ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "codex_git execution requires --codex-worker-config".into(),
                    )
                }),
        }
    }

    pub(crate) fn execute(
        &self,
        assignment: &AssignmentContext,
    ) -> Result<CandidateResult, TyrionError> {
        let control = self.control(&assignment.attempt_id)?;
        let result = (|| {
            let configuration: routing::WorkerConfiguration =
                serde_json::from_value(assignment.selected_configuration.clone())?;
            if self.hold_for_control
                && matches!(&assignment.execution, ExecutionSpec::Deterministic)
            {
                let mut clarifications = control.clarifications.lock().map_err(|_| {
                    TyrionError::InvalidRequest(
                        "Worker clarification channel is unavailable".into(),
                    )
                })?;
                while !control.interrupted.load(Ordering::SeqCst) {
                    let (next, _) = control
                        .changed
                        .wait_timeout(clarifications, Duration::from_millis(50))
                        .map_err(|_| {
                            TyrionError::InvalidRequest(
                                "Worker control channel is unavailable".into(),
                            )
                        })?;
                    clarifications = next;
                }
            }
            if control.interrupted.load(Ordering::SeqCst) {
                return Err(TyrionError::WorkerInterrupted);
            }
            match (configuration.adapter.kind, &assignment.execution) {
                (
                    routing::WorkerAdapterKind::CodexAppServer
                    | routing::WorkerAdapterKind::ClaudeAgentSdk,
                    ExecutionSpec::Deterministic,
                ) => {
                    let runtime = self.contained_codex.as_ref().ok_or_else(|| {
                        TyrionError::InvalidRequest(
                            "structured Worker execution requires --codex-worker-config for containment"
                                .into(),
                        )
                    })?;
                    let report = structured_process::execute(
                        runtime,
                        &configuration,
                        assignment,
                        &control,
                        None,
                    )?;
                    let output = report.result_summary;
                    Ok(CandidateResult {
                        artifact_revision: ArtifactRevision::for_content(&output),
                        output,
                        base_revision: None,
                        candidate_commits: Vec::new(),
                        changed_paths: Vec::new(),
                        artifacts: Vec::new(),
                        known_effects: Vec::new(),
                        native_session_id: Some(report.native_session_id),
                        usage: serde_json::json!({
                            "input_tokens": report.input_tokens,
                            "output_tokens": report.output_tokens,
                            "cost_cents": report.cost_cents,
                        }),
                        latest_meaningful_activity: report.latest_meaningful_activity,
                        state: CandidateState::Deterministic,
                    })
                }
                (
                    routing::WorkerAdapterKind::CodexAppServer
                    | routing::WorkerAdapterKind::ClaudeAgentSdk,
                    ExecutionSpec::CodexGit {
                        repository,
                        base_revision,
                    },
                ) => {
                    let runtime = self.contained_codex.as_ref().ok_or_else(|| {
                        TyrionError::InvalidRequest(
                            "structured codex_git execution requires --codex-worker-config for Git artifact containment and verification"
                                .into(),
                        )
                    })?;
                    let prepared = runtime.prepare_structured_git_attempt(
                        assignment,
                        Path::new(repository),
                        base_revision,
                    )?;
                    let report = structured_process::execute(
                        runtime,
                        &configuration,
                        assignment,
                        &control,
                        Some(&prepared),
                    )?;
                    let candidate = runtime.accept_structured_git_candidate(
                        assignment,
                        prepared,
                        report.result_summary,
                    )?;
                    Ok(CandidateResult {
                        output: candidate.output.clone(),
                        artifact_revision: ArtifactRevision::from_claim(
                            candidate.candidate_revision.clone(),
                        ),
                        base_revision: Some(base_revision.clone()),
                        candidate_commits: candidate.candidate_commits.clone(),
                        changed_paths: candidate.changed_paths.clone(),
                        artifacts: candidate.artifacts.clone(),
                        known_effects: candidate.known_effects.clone(),
                        native_session_id: Some(report.native_session_id),
                        usage: serde_json::json!({
                            "input_tokens": report.input_tokens,
                            "output_tokens": report.output_tokens,
                            "cost_cents": report.cost_cents,
                        }),
                        latest_meaningful_activity: report.latest_meaningful_activity,
                        state: CandidateState::CodexGit(candidate.state),
                    })
                }
                (routing::WorkerAdapterKind::DeterministicLocal, ExecutionSpec::Deterministic) => {
                    let output = if self.incorrect_first_result {
                    let mut commissions =
                        self.incorrect_result_commissions.lock().map_err(|_| {
                            TyrionError::InvalidRequest(
                                "fault-injection Worker state is unavailable".into(),
                            )
                        })?;
                    if commissions.insert(assignment.commission_id.clone()) {
                        "insufficient worker result".into()
                    } else {
                        assignment.goal.clone()
                    }
                } else {
                    assignment.goal.clone()
                };
                let artifact_revision = if self.corrupt_artifact_revision {
                    ArtifactRevision::from_claim("sha256:forged")
                } else {
                    ArtifactRevision::for_content(&output)
                };
                Ok(CandidateResult {
                    output,
                    artifact_revision,
                    base_revision: None,
                    candidate_commits: Vec::new(),
                    changed_paths: Vec::new(),
                        artifacts: Vec::new(),
                        known_effects: Vec::new(),
                        native_session_id: None,
                        usage: serde_json::json!({}),
                        latest_meaningful_activity: "Deterministic Result submitted".into(),
                        state: CandidateState::Deterministic,
                    })
                }
                (
                    routing::WorkerAdapterKind::ContainedCodex,
                    ExecutionSpec::CodexGit {
                        repository,
                        base_revision,
                    },
                ) => {
                let runtime = self.contained_codex.as_ref().ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "codex_git execution requires --codex-worker-config".into(),
                    )
                })?;
                let candidate =
                    runtime.execute(assignment, Path::new(repository), base_revision)?;
                Ok(CandidateResult {
                    output: candidate.output.clone(),
                    artifact_revision: ArtifactRevision::from_claim(
                        candidate.candidate_revision.clone(),
                    ),
                    base_revision: Some(base_revision.clone()),
                    candidate_commits: candidate.candidate_commits.clone(),
                    changed_paths: candidate.changed_paths.clone(),
                        artifacts: candidate.artifacts.clone(),
                        known_effects: candidate.known_effects.clone(),
                        native_session_id: None,
                        usage: serde_json::json!({}),
                        latest_meaningful_activity: "Contained Codex Result submitted".into(),
                        state: CandidateState::CodexGit(candidate.state),
                    })
                }
                _ => Err(TyrionError::InvalidRequest(format!(
                    "selected Worker Configuration {} adapter is incompatible with the Assignment execution",
                    configuration.id
                ))),
            }
        })();
        let interrupted = control.interrupted.load(Ordering::SeqCst);
        if interrupted {
            let watchdog_signal = *control.watchdog_signal.lock().map_err(|_| {
                TyrionError::InvalidRequest("Watchdog signal registry is unavailable".into())
            })?;
            if let Some(signal) = watchdog_signal {
                Err(TyrionError::WatchdogContained { signal })
            } else {
                Err(TyrionError::WorkerInterrupted)
            }
        } else {
            result
        }
    }

    pub(crate) fn verify_candidate(
        &self,
        assignment: &AssignmentContext,
        candidate: &CandidateResult,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        match &candidate.state {
            CandidateState::Deterministic => Ok(verify_deterministic(
                assignment,
                &candidate.output,
                &candidate.artifact_revision,
                VerificationScope::Candidate,
            )),
            CandidateState::CodexGit(state) => self
                .contained_codex
                .as_ref()
                .expect("codex candidate requires its runtime")
                .verify_candidate(assignment, state),
        }
    }

    pub(crate) fn integrate(
        &self,
        assignment: &AssignmentContext,
        candidate: &CandidateResult,
    ) -> Result<IntegratedResult, TyrionError> {
        match &candidate.state {
            CandidateState::Deterministic => Ok(IntegratedResult {
                artifact_revision: candidate.artifact_revision.clone(),
                artifacts: Vec::new(),
                state: IntegratedState::Deterministic(candidate.output.clone()),
            }),
            CandidateState::CodexGit(state) => {
                let integrated = self
                    .contained_codex
                    .as_ref()
                    .expect("codex candidate requires its runtime")
                    .integrate(assignment, state)?;
                Ok(IntegratedResult {
                    artifact_revision: ArtifactRevision::from_claim(
                        integrated.integrated_revision.clone(),
                    ),
                    artifacts: integrated.artifacts.clone(),
                    state: IntegratedState::CodexGit(integrated.state),
                })
            }
        }
    }

    pub(crate) fn wait_before_integration(&self) {
        if self.hold_before_integration {
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    pub(crate) fn wait_after_integration(&self) {
        if self.hold_after_integration {
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    pub(crate) fn wait_after_external_integration(&self) {
        if self.hold_after_external_integration {
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    pub(crate) fn verify_integrated(
        &self,
        assignment: &AssignmentContext,
        integrated: &IntegratedResult,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        match &integrated.state {
            IntegratedState::Deterministic(output) => Ok(verify_deterministic(
                assignment,
                output,
                &integrated.artifact_revision,
                VerificationScope::Integrated,
            )),
            IntegratedState::CodexGit(state) => self
                .contained_codex
                .as_ref()
                .expect("integrated codex Result requires its runtime")
                .verify_integrated(assignment, state),
        }
    }

    pub(crate) fn rollback_integration(
        &self,
        integrated: &IntegratedResult,
    ) -> Result<(), TyrionError> {
        match &integrated.state {
            IntegratedState::Deterministic(_) => Ok(()),
            IntegratedState::CodexGit(state) => self
                .contained_codex
                .as_ref()
                .expect("integrated codex Result requires its runtime")
                .rollback_integration(state),
        }
    }
}

fn current_time_millis() -> Result<i64, TyrionError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TyrionError::InvalidRequest("system clock is before Unix epoch".into()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| TyrionError::InvalidRequest("system clock does not fit in SQLite".into()))
}

fn verify_deterministic(
    assignment: &AssignmentContext,
    output: &str,
    artifact_revision: &ArtifactRevision,
    scope: VerificationScope,
) -> Vec<VerificationRecord> {
    assignment
        .criteria
        .iter()
        .filter(|criterion| criterion.verifier_type == VerifierType::Deterministic)
        .flat_map(|criterion| {
            let Verifier::ExactMatch { expected } = &criterion.verifier else {
                unreachable!("validated deterministic criterion uses exact_match")
            };
            let artifact_is_current = artifact_revision.matches_content(output);
            let outcome = if artifact_is_current && expected == output {
                EvidenceOutcome::Passed
            } else {
                EvidenceOutcome::Failed
            };
            (0..criterion.verification_depth.required_passes()).map(move |index| {
                VerificationRecord {
                    criterion_id: criterion.id.clone(),
                    evidence_type: criterion.required_evidence.clone(),
                    verifier_type: criterion.verifier_type,
                    verification_attempt_id: uuid::Uuid::new_v4().to_string(),
                    verifier_identity: format!("deterministic-exact-match-{}", index + 1),
                    verifier_configuration: criterion.verifier_configuration.clone(),
                    verifier_kind: VerificationKind::ExactMatch,
                    procedure: criterion.verifier.clone(),
                    environment: criterion.verification_environment.clone(),
                    scope,
                    outcome,
                    observed: output.to_owned(),
                    expected: expected.clone(),
                    material_contradiction: false,
                    defect: (outcome == EvidenceOutcome::Failed)
                        .then_some(VerificationDefect::Result),
                    producer_attempt_id: Some(assignment.attempt_id.clone()),
                }
            })
        })
        .collect()
}

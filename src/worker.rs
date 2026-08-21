use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::artifact::ArtifactRevision;
use crate::domain::EvidenceOutcome;
use crate::protocol::{
    ExecutionSpec, VerificationDefect, VerificationDepth, Verifier, VerifierType,
};
use crate::TyrionError;

mod contained_codex;

pub const DETERMINISTIC_ACTION: &str = "deterministic.echo";
pub const CODEX_GIT_ACTION: &str = "codex.git_change";

pub(crate) struct WorkerRuntime {
    contained_codex: Option<contained_codex::ContainedCodexRuntime>,
    corrupt_artifact_revision: bool,
    incorrect_first_result: bool,
    incorrect_result_commissions: Mutex<std::collections::HashSet<String>>,
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
    pub criteria: Vec<CriterionDefinition>,
    pub authorized_paths: Vec<String>,
    pub declared_write_scopes: Vec<String>,
    pub comparison_candidates: Vec<ComparisonCandidate>,
    pub max_storage_bytes: u64,
    pub lease_expires_at: i64,
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

#[derive(Clone)]
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
        corrupt_artifact_revision: bool,
        incorrect_first_result: bool,
    ) -> Result<Self, TyrionError> {
        let contained_codex = codex_worker_config
            .map(|path| contained_codex::ContainedCodexRuntime::load(path, data_dir))
            .transpose()?;
        Ok(Self {
            contained_codex,
            corrupt_artifact_revision,
            incorrect_first_result,
            incorrect_result_commissions: Mutex::new(std::collections::HashSet::new()),
        })
    }

    pub(crate) fn configuration(&self, execution: &ExecutionSpec) -> Result<String, TyrionError> {
        match execution {
            ExecutionSpec::Deterministic if self.corrupt_artifact_revision => {
                Ok("fault-corrupt-artifact-revision-v1".into())
            }
            ExecutionSpec::Deterministic => Ok("deterministic-local-v1".into()),
            ExecutionSpec::CodexGit { .. } => self
                .contained_codex
                .as_ref()
                .map(contained_codex::ContainedCodexRuntime::configuration)
                .ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "codex_git execution requires --codex-worker-config".into(),
                    )
                }),
        }
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
        match &assignment.execution {
            ExecutionSpec::Deterministic => {
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
                    state: CandidateState::Deterministic,
                })
            }
            ExecutionSpec::CodexGit {
                repository,
                base_revision,
            } => {
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
                    state: CandidateState::CodexGit(candidate.state),
                })
            }
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

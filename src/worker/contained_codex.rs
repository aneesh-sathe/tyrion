use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    ArtifactRecord, AssignmentContext, CriterionDefinition, VerificationKind, VerificationRecord,
    VerificationScope,
};
use crate::domain::EvidenceOutcome;
use crate::error::IntegrationFailureKind;
use crate::protocol::Verifier;
use crate::TyrionError;

const SOURCE_REVISION: &str = "dd2b4e3bc0688bdd59f90030f7c1d52511d6e354";
const SOURCE_PATCH_SHA256: &str =
    "6452fbe2836ffbe43e0e73c813db5dc5dda7ee70537b7033fc5429573160e402";
const BASE_IMAGE: &str = "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e";
const POLICY_SHA256: &str = "76715da36c5e5f8603cd4732690707bca8b7f11ee153ae36521028db75bc4453";
const CLAUDE_POLICY_SHA256: &str =
    "89ec4d87f6a6b4bd8c581ec878ff82cb2e2acf96a5a581e94e3f0b10d84feccf";
const PI_POLICY_SHA256: &str = "c7bbd0d358df5d7943e38d32ea2442b2f4cba8e34cdd39db98982e5502f62982";
const CODEX_VERSION: &str = "codex-cli 0.147.0";

#[derive(Debug, Deserialize)]
struct RuntimeConfig {
    openshell_binary: PathBuf,
    openshell_sha256: String,
    openshell_version: String,
    openshell_config_home: PathBuf,
    policy_path: PathBuf,
    policy_sha256: String,
    gateway_config_path: PathBuf,
    gateway_config_sha256: String,
    kernel_config_path: PathBuf,
    kernel_config_sha256: String,
    runtime_artifacts: Vec<PinnedArtifact>,
    source_revision: String,
    source_patch_path: PathBuf,
    source_patch_sha256: String,
    base_image: String,
    codex_binary: PathBuf,
    codex_version: String,
    codex_sha256: String,
    model: String,
    openshell_provider: String,
    #[serde(default)]
    claude: Option<ClaudeRuntimeConfig>,
    #[serde(default)]
    pi: Option<PiRuntimeConfig>,
    lease_ttl_seconds: u64,
    vcpus: u32,
    memory_mib: u64,
    overlay_disk_mib: u64,
    max_processes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeRuntimeConfig {
    policy_path: PathBuf,
    policy_sha256: String,
    openshell_provider: String,
    binary: PathBuf,
    version: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiRuntimeConfig {
    policy_path: PathBuf,
    policy_sha256: String,
    openshell_provider: String,
    model_provider: String,
    model: String,
    binary: PathBuf,
    version: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct PinnedArtifact {
    path: PathBuf,
    sha256: String,
}

pub(super) struct ContainedCodexRuntime {
    config: RuntimeConfig,
    data_dir: PathBuf,
    fingerprint: String,
}

pub(super) struct GitCandidate {
    pub output: String,
    pub candidate_revision: String,
    pub candidate_commits: Vec<String>,
    pub changed_paths: Vec<String>,
    pub artifacts: Vec<ArtifactRecord>,
    pub known_effects: Vec<String>,
    pub state: GitCandidateState,
}

pub(super) struct GitCandidateState {
    base_bundle: PathBuf,
    candidate_bundle: PathBuf,
    base_revision: String,
    candidate_revision: String,
    candidate_commits: Vec<String>,
    read_only: bool,
}

pub(super) struct StructuredGitAttempt {
    artifact_dir: PathBuf,
    base_bundle: PathBuf,
    candidate_bundle: PathBuf,
    base_revision: String,
}

pub(super) struct StructuredAdapterSandbox<'a> {
    sandbox: Option<Sandbox<'a>>,
}

struct StructuredRuntimeProfile<'a> {
    policy_path: &'a Path,
    provider: &'a str,
    binary: &'a Path,
    remote_binary: &'static str,
    binary_environment: &'static str,
    version: &'a str,
}

impl StructuredGitAttempt {
    pub(super) fn launch_payload(&self) -> Value {
        serde_json::json!({
            "base_bundle": "/sandbox/base.bundle",
            "candidate_bundle": "/sandbox/candidate.bundle",
            "base_revision": self.base_revision,
            "candidate_reference": "refs/heads/tyrion-result",
        })
    }
}

impl StructuredAdapterSandbox<'_> {
    pub(super) fn command(
        &self,
        configuration: &super::routing::WorkerConfiguration,
        assignment: &AssignmentContext,
        git_attempt: Option<&StructuredGitAttempt>,
        configuration_fingerprint: &str,
    ) -> Result<Command, TyrionError> {
        let sandbox = self.sandbox.as_ref().ok_or_else(|| {
            TyrionError::InvalidRequest("structured adapter sandbox is no longer active".into())
        })?;
        let mut command = Command::new(&sandbox.runtime.config.openshell_binary);
        command
            .args([
                "sandbox",
                "exec",
                "-n",
                &sandbox.name,
                "--workdir",
                "/sandbox",
                "--no-tty",
                "--",
                "env",
                "PATH=/usr/local/bin:/usr/bin:/bin",
            ])
            .arg(format!("TYRION_COMMISSION_ID={}", assignment.commission_id))
            .arg(format!("TYRION_ASSIGNMENT_ID={}", assignment.assignment_id))
            .arg(format!("TYRION_ATTEMPT_ID={}", assignment.attempt_id))
            .arg(format!(
                "TYRION_MANDATE_REVISION={}",
                assignment.mandate_revision
            ))
            .arg(format!("TYRION_PLAN_REVISION={}", assignment.plan_revision))
            .arg(format!(
                "TYRION_CONFIGURATION_FINGERPRINT={configuration_fingerprint}"
            ))
            .arg("TYRION_WORKSPACE_ROOT=/sandbox")
            .arg(match configuration.adapter.kind {
                super::routing::WorkerAdapterKind::CodexAppServer => {
                    "TYRION_CODEX_BINARY=/sandbox/codex"
                }
                super::routing::WorkerAdapterKind::ClaudeAgentSdk => {
                    "TYRION_CLAUDE_BINARY=/sandbox/claude"
                }
                super::routing::WorkerAdapterKind::PiRpc => "TYRION_PI_BINARY=/sandbox/pi",
                _ => unreachable!("structured sandbox command uses a structured adapter"),
            })
            .args(git_attempt.into_iter().flat_map(|attempt| {
                [
                    "TYRION_BASE_BUNDLE=/sandbox/base.bundle".to_owned(),
                    "TYRION_CANDIDATE_BUNDLE=/sandbox/candidate.bundle".to_owned(),
                    format!("TYRION_BASE_REVISION={}", attempt.base_revision),
                ]
            }))
            .arg("/sandbox/worker-adapter")
            .args(configuration.adapter.command.iter().skip(1))
            .env_clear()
            .env(
                "XDG_CONFIG_HOME",
                &sandbox.runtime.config.openshell_config_home,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    pub(super) fn finish(
        &mut self,
        git_attempt: Option<&StructuredGitAttempt>,
        deadline: i64,
    ) -> Result<(), TyrionError> {
        let Some(sandbox) = self.sandbox.take() else {
            return Ok(());
        };
        if let Some(git_attempt) = git_attempt {
            sandbox.download(
                "/sandbox/candidate.bundle",
                &git_attempt.candidate_bundle,
                deadline,
            )?;
        }
        sandbox.delete()
    }

    pub(super) fn terminate(&mut self) {
        if let Some(sandbox) = self.sandbox.take() {
            let _ = sandbox.delete();
        }
    }
}

impl Drop for StructuredAdapterSandbox<'_> {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(super) struct GitIntegrated {
    pub integrated_revision: String,
    pub artifacts: Vec<ArtifactRecord>,
    pub state: GitIntegratedState,
}

pub(super) struct GitIntegratedState {
    integrated_bundle: PathBuf,
    integration_repository: PathBuf,
    _integration_lock: File,
    previous_revision: String,
}

impl ContainedCodexRuntime {
    pub(super) fn load(config_path: &Path, data_dir: &Path) -> Result<Self, TyrionError> {
        let encoded = fs::read(config_path)?;
        let config: RuntimeConfig = serde_json::from_slice(&encoded)?;
        validate_config(&config)?;
        let fingerprint = format!("{:x}", Sha256::digest(&encoded));
        Ok(Self {
            config,
            data_dir: data_dir.to_owned(),
            fingerprint,
        })
    }

    pub(super) fn routing_descriptor(&self) -> super::routing::ContainedCodexDescriptor {
        let mut settings = std::collections::BTreeMap::new();
        settings.insert(
            "openshell_version".into(),
            serde_json::json!(self.config.openshell_version),
        );
        settings.insert(
            "source_revision".into(),
            serde_json::json!(self.config.source_revision),
        );
        settings.insert(
            "base_image".into(),
            serde_json::json!(self.config.base_image),
        );
        settings.insert("vcpus".into(), serde_json::json!(self.config.vcpus));
        settings.insert(
            "memory_mib".into(),
            serde_json::json!(self.config.memory_mib),
        );
        settings.insert(
            "overlay_disk_mib".into(),
            serde_json::json!(self.config.overlay_disk_mib),
        );
        settings.insert(
            "max_processes".into(),
            serde_json::json!(self.config.max_processes),
        );
        settings.insert(
            "runtime_configuration_sha256".into(),
            serde_json::json!(self.fingerprint),
        );
        super::routing::ContainedCodexDescriptor {
            id: format!("contained-codex-{}", &self.fingerprint[..16]),
            version: self.config.codex_version.clone(),
            model: self.config.model.clone(),
            settings,
            max_storage_bytes: self
                .config
                .overlay_disk_mib
                .saturating_mul(1024)
                .saturating_mul(1024),
            containment_profile: format!("openshell-repaired-v0.0.104-{}", &self.fingerprint[..16]),
            supports_claude: self.config.claude.is_some(),
            supports_pi: self.config.pi.is_some(),
            pi_model_provider: self.config.pi.as_ref().map(|pi| pi.model_provider.clone()),
            pi_model: self.config.pi.as_ref().map(|pi| pi.model.clone()),
        }
    }

    pub(super) fn lease_ttl_seconds(&self) -> u64 {
        self.config.lease_ttl_seconds
    }

    pub(super) fn integration_repository(&self, commission_id: &str) -> PathBuf {
        self.data_dir
            .join("integrations")
            .join(commission_id)
            .join("repository")
    }

    pub(super) fn prepare_structured_adapter_sandbox(
        &self,
        configuration: &super::routing::WorkerConfiguration,
        assignment: &AssignmentContext,
        git_attempt: Option<&StructuredGitAttempt>,
    ) -> Result<StructuredAdapterSandbox<'_>, TyrionError> {
        let sandbox_name = sandbox_name("adapter", &assignment.attempt_id);
        let profile = self.structured_runtime_profile(configuration.adapter.kind)?;
        let sandbox = Sandbox::create(
            self,
            &sandbox_name,
            profile.policy_path,
            profile.provider,
            assignment.lease_expires_at,
        )?;
        let host_scope = match &assignment.execution {
            crate::protocol::ExecutionSpec::CodexGit { repository, .. } => Path::new(repository),
            crate::protocol::ExecutionSpec::Deterministic => &self.data_dir,
        };
        sandbox.preflight(host_scope, &self.data_dir, assignment.lease_expires_at)?;
        sandbox.upload(
            Path::new(&configuration.adapter.command[0]),
            "/sandbox/worker-adapter",
            assignment.lease_expires_at,
        )?;
        sandbox.upload(
            profile.binary,
            profile.remote_binary,
            assignment.lease_expires_at,
        )?;
        sandbox.exec_checked(
            &[
                "chmod",
                "700",
                "/sandbox/worker-adapter",
                profile.remote_binary,
            ],
            None,
            assignment.lease_expires_at,
        )?;
        let version = sandbox.exec_checked(
            &[profile.remote_binary, "--version"],
            None,
            assignment.lease_expires_at,
        )?;
        if String::from_utf8_lossy(&version.stdout).trim() != profile.version {
            return Err(TyrionError::InvalidRequest(format!(
                "{} binary version does not match its runtime pin",
                profile.binary_environment
            )));
        }
        if let Some(git_attempt) = git_attempt {
            sandbox.upload(
                &git_attempt.base_bundle,
                "/sandbox/base.bundle",
                assignment.lease_expires_at,
            )?;
        }
        Ok(StructuredAdapterSandbox {
            sandbox: Some(sandbox),
        })
    }

    fn structured_runtime_profile(
        &self,
        kind: super::routing::WorkerAdapterKind,
    ) -> Result<StructuredRuntimeProfile<'_>, TyrionError> {
        match kind {
            super::routing::WorkerAdapterKind::CodexAppServer => Ok(StructuredRuntimeProfile {
                policy_path: &self.config.policy_path,
                provider: &self.config.openshell_provider,
                binary: &self.config.codex_binary,
                remote_binary: "/sandbox/codex",
                binary_environment: "Codex",
                version: &self.config.codex_version,
            }),
            super::routing::WorkerAdapterKind::ClaudeAgentSdk => {
                let claude = self.config.claude.as_ref().ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "Claude Worker execution requires a pinned Claude OpenShell profile".into(),
                    )
                })?;
                Ok(StructuredRuntimeProfile {
                    policy_path: &claude.policy_path,
                    provider: &claude.openshell_provider,
                    binary: &claude.binary,
                    remote_binary: "/sandbox/claude",
                    binary_environment: "Claude",
                    version: &claude.version,
                })
            }
            super::routing::WorkerAdapterKind::PiRpc => {
                let pi = self.config.pi.as_ref().ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "Pi Worker execution requires a pinned Pi OpenShell profile".into(),
                    )
                })?;
                Ok(StructuredRuntimeProfile {
                    policy_path: &pi.policy_path,
                    provider: &pi.openshell_provider,
                    binary: &pi.binary,
                    remote_binary: "/sandbox/pi",
                    binary_environment: "Pi",
                    version: &pi.version,
                })
            }
            _ => Err(TyrionError::InvalidRequest(
                "structured runtime profile requested for an unsupported adapter".into(),
            )),
        }
    }

    pub(super) fn cleanup_stranded_attempt(&self, attempt_id: &str) -> Result<(), TyrionError> {
        let mut sandboxes = vec![
            sandbox_name("adapter", attempt_id),
            sandbox_name("attempt", attempt_id),
        ];
        for scope in ["candidate", "integrated"] {
            for verification_index in 1..=2 {
                sandboxes.push(sandbox_name(
                    scope,
                    &format!("{attempt_id}-verification-{verification_index}"),
                ));
            }
        }
        for name in sandboxes {
            let output = Command::new(&self.config.openshell_binary)
                .args(["sandbox", "delete", name.as_str()])
                .env_clear()
                .env("XDG_CONFIG_HOME", &self.config.openshell_config_home)
                .output()?;
            require_success("stranded OpenShell sandbox cleanup", output)?;
        }
        Ok(())
    }

    pub(super) fn restore_integration_repository(
        &self,
        commission_id: &str,
        durable_revision: &str,
    ) -> Result<(), TyrionError> {
        let integration_root = self.data_dir.join("integrations").join(commission_id);
        let repository = integration_root.join("repository");
        if !repository.exists() {
            return Ok(());
        }
        let integration_lock_path = integration_root.join("integration.lock");
        let integration_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&integration_lock_path)?;
        fs::set_permissions(&integration_lock_path, fs::Permissions::from_mode(0o600))?;
        integration_lock.lock_exclusive()?;
        git_checked(
            Some(&repository),
            &[os("reset"), os("--hard"), os(durable_revision)],
        )?;
        git_checked(
            Some(&repository),
            &[
                os("branch"),
                os("-f"),
                os("tyrion-integration"),
                os(durable_revision),
            ],
        )?;
        Ok(())
    }

    pub(super) fn prepare_structured_git_attempt(
        &self,
        assignment: &AssignmentContext,
        repository: &Path,
        base_revision: &str,
    ) -> Result<StructuredGitAttempt, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let repository = repository.canonicalize()?;
        let artifact_dir = self
            .data_dir
            .join("artifacts")
            .join(&assignment.commission_id)
            .join(&assignment.attempt_id);
        create_private_dir(&artifact_dir)?;
        let base_bundle = artifact_dir.join("base.bundle");
        let candidate_bundle = artifact_dir.join("candidate.bundle");
        create_base_bundle(&repository, base_revision, &base_bundle)?;
        let mut input_bundles = vec![base_bundle.as_path()];
        input_bundles.extend(
            assignment
                .comparison_candidates
                .iter()
                .map(|candidate| candidate.bundle_path.as_path()),
        );
        enforce_storage_ceiling(&input_bundles, assignment.max_storage_bytes)?;
        Ok(StructuredGitAttempt {
            artifact_dir,
            base_bundle,
            candidate_bundle,
            base_revision: base_revision.to_owned(),
        })
    }

    pub(super) fn accept_structured_git_candidate(
        &self,
        assignment: &AssignmentContext,
        prepared: StructuredGitAttempt,
        output: String,
    ) -> Result<GitCandidate, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let base_artifact = artifact("base_git_bundle", &prepared.base_bundle)?;
        let candidate_artifact = artifact("candidate_git_bundle", &prepared.candidate_bundle)?;
        let mut stored_bundles = vec![
            prepared.base_bundle.as_path(),
            prepared.candidate_bundle.as_path(),
        ];
        stored_bundles.extend(
            assignment
                .comparison_candidates
                .iter()
                .map(|candidate| candidate.bundle_path.as_path()),
        );
        enforce_storage_ceiling(&stored_bundles, assignment.max_storage_bytes)?;
        let validated = validate_candidate_bundle(
            &prepared.artifact_dir,
            &prepared.base_bundle,
            &prepared.candidate_bundle,
            &prepared.base_revision,
            &assignment.authorized_paths,
            assignment.declared_write_scopes.is_empty(),
        )?;
        Ok(GitCandidate {
            output,
            candidate_revision: validated.candidate_revision.clone(),
            candidate_commits: validated.commits.clone(),
            changed_paths: validated.changed_paths,
            artifacts: vec![base_artifact, candidate_artifact],
            known_effects: Vec::new(),
            state: GitCandidateState {
                base_bundle: prepared.base_bundle,
                candidate_bundle: prepared.candidate_bundle,
                base_revision: prepared.base_revision,
                candidate_revision: validated.candidate_revision,
                candidate_commits: validated.commits,
                read_only: assignment.declared_write_scopes.is_empty(),
            },
        })
    }

    pub(super) fn execute(
        &self,
        assignment: &AssignmentContext,
        repository: &Path,
        base_revision: &str,
    ) -> Result<GitCandidate, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let repository = repository.canonicalize()?;
        let artifact_dir = self
            .data_dir
            .join("artifacts")
            .join(&assignment.commission_id)
            .join(&assignment.attempt_id);
        create_private_dir(&artifact_dir)?;
        let base_bundle = artifact_dir.join("base.bundle");
        create_base_bundle(&repository, base_revision, &base_bundle)?;
        let base_artifact = artifact("base_git_bundle", &base_bundle)?;
        let mut input_bundles = vec![base_bundle.as_path()];
        input_bundles.extend(
            assignment
                .comparison_candidates
                .iter()
                .map(|candidate| candidate.bundle_path.as_path()),
        );
        enforce_storage_ceiling(&input_bundles, assignment.max_storage_bytes)?;

        let sandbox_name = sandbox_name("attempt", &assignment.attempt_id);
        let sandbox = Sandbox::create(
            self,
            &sandbox_name,
            &self.config.policy_path,
            &self.config.openshell_provider,
            assignment.lease_expires_at,
        )?;
        sandbox.preflight(&repository, &self.data_dir, assignment.lease_expires_at)?;
        sandbox.upload(
            &base_bundle,
            "/sandbox/base.bundle",
            assignment.lease_expires_at,
        )?;
        for (index, contender) in assignment.comparison_candidates.iter().enumerate() {
            sandbox.upload(
                &contender.bundle_path,
                &format!("/sandbox/contenders/{index}.bundle"),
                assignment.lease_expires_at,
            )?;
        }
        sandbox.upload(
            &self.config.codex_binary,
            "/sandbox/codex",
            assignment.lease_expires_at,
        )?;
        sandbox.exec_checked(
            &["chmod", "700", "/sandbox/codex"],
            None,
            assignment.lease_expires_at,
        )?;
        let codex = sandbox.exec_checked(
            &["/sandbox/codex", "--version"],
            None,
            assignment.lease_expires_at,
        )?;
        if String::from_utf8_lossy(&codex.stdout).trim() != self.config.codex_version {
            return Err(TyrionError::InvalidRequest(
                "Codex binary version does not match its pin".into(),
            ));
        }
        let prompt_path = artifact_dir.join("prompt.txt");
        fs::write(&prompt_path, worker_prompt(assignment, base_revision))?;
        sandbox.upload(
            &prompt_path,
            "/sandbox/prompt.txt",
            assignment.lease_expires_at,
        )?;
        let schema_path = artifact_dir.join("result-schema.json");
        fs::write(&schema_path, result_schema())?;
        sandbox.upload(
            &schema_path,
            "/sandbox/result-schema.json",
            assignment.lease_expires_at,
        )?;
        let attempt_script_path = artifact_dir.join("run-attempt.sh");
        fs::write(
            &attempt_script_path,
            attempt_script(
                base_revision,
                &self.config.model,
                assignment.declared_write_scopes.is_empty(),
            ),
        )?;
        fs::set_permissions(&attempt_script_path, fs::Permissions::from_mode(0o700))?;
        sandbox.upload(
            &attempt_script_path,
            "/sandbox/run-attempt.sh",
            assignment.lease_expires_at,
        )?;
        sandbox.exec_checked(
            &["sh", "/sandbox/run-attempt.sh"],
            None,
            assignment.lease_expires_at,
        )?;

        let candidate_bundle = artifact_dir.join("candidate.bundle");
        let result_path = artifact_dir.join("codex-result.json");
        sandbox.download(
            "/sandbox/candidate.bundle",
            &candidate_bundle,
            assignment.lease_expires_at,
        )?;
        sandbox.download(
            "/sandbox/codex-result.json",
            &result_path,
            assignment.lease_expires_at,
        )?;
        sandbox.delete()?;

        let candidate_artifact = artifact("candidate_git_bundle", &candidate_bundle)?;
        let mut stored_bundles = vec![base_bundle.as_path(), candidate_bundle.as_path()];
        stored_bundles.extend(
            assignment
                .comparison_candidates
                .iter()
                .map(|candidate| candidate.bundle_path.as_path()),
        );
        enforce_storage_ceiling(&stored_bundles, assignment.max_storage_bytes)?;
        let validated = validate_candidate_bundle(
            &artifact_dir,
            &base_bundle,
            &candidate_bundle,
            base_revision,
            &assignment.authorized_paths,
            assignment.declared_write_scopes.is_empty(),
        )?;
        let codex_result: Value = serde_json::from_slice(&fs::read(result_path)?)?;
        let output = codex_result
            .get("summary")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "Codex structured Result is missing a string summary".into(),
                )
            })?
            .to_owned();
        let known_effects = codex_result
            .get("known_effects")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TyrionError::InvalidRequest(
                    "Codex structured Result is missing known_effects".into(),
                )
            })?
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "Codex known_effects entries must be strings".into(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !known_effects.is_empty() {
            return Err(TyrionError::InvalidRequest(
                "the contained Codex slice does not permit external effects".into(),
            ));
        }

        Ok(GitCandidate {
            output,
            candidate_revision: validated.candidate_revision.clone(),
            candidate_commits: validated.commits.clone(),
            changed_paths: validated.changed_paths,
            artifacts: vec![base_artifact, candidate_artifact],
            known_effects,
            state: GitCandidateState {
                base_bundle,
                candidate_bundle,
                base_revision: base_revision.to_owned(),
                candidate_revision: validated.candidate_revision,
                candidate_commits: validated.commits,
                read_only: assignment.declared_write_scopes.is_empty(),
            },
        })
    }

    pub(super) fn verify_candidate(
        &self,
        assignment: &AssignmentContext,
        candidate: &GitCandidateState,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        self.verify_bundles(
            assignment,
            VerificationScope::Candidate,
            Some((&candidate.base_bundle, &candidate.base_revision)),
            &candidate.candidate_bundle,
            &candidate.candidate_revision,
        )
    }

    pub(super) fn integrate(
        &self,
        assignment: &AssignmentContext,
        candidate: &GitCandidateState,
    ) -> Result<GitIntegrated, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let integration_root = self
            .data_dir
            .join("integrations")
            .join(&assignment.commission_id);
        create_private_dir(&integration_root)?;
        let integration_lock_path = integration_root.join("integration.lock");
        let integration_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&integration_lock_path)?;
        fs::set_permissions(&integration_lock_path, fs::Permissions::from_mode(0o600))?;
        integration_lock.lock_exclusive()?;
        let repository = integration_root.join("repository");
        if !repository.exists() {
            git_checked(
                None,
                &[
                    os("clone"),
                    os("--quiet"),
                    candidate.base_bundle.as_os_str().to_owned(),
                    repository.as_os_str().to_owned(),
                ],
            )?;
            git_checked(
                Some(&repository),
                &[
                    os("checkout"),
                    os("--quiet"),
                    os("--detach"),
                    os(&candidate.base_revision),
                ],
            )?;
        }
        let current = git_text(&repository, &[os("rev-parse"), os("HEAD")])?;
        let previous_revision = current.trim().to_owned();
        let base_is_ancestor = git_output(
            Some(&repository),
            &[
                os("merge-base"),
                os("--is-ancestor"),
                os(&candidate.base_revision),
                os(&previous_revision),
            ],
        )?;
        if !base_is_ancestor.status.success() {
            return Err(TyrionError::IntegrationFailure {
                kind: IntegrationFailureKind::StaleBase,
                message: format!(
                    "authoritative Integration is {previous_revision}, but the Result base {} is not its ancestor",
                    candidate.base_revision
                ),
            });
        }
        if !candidate.read_only {
            git_checked(
                Some(&repository),
                &[
                    os("fetch"),
                    os("--quiet"),
                    candidate.candidate_bundle.as_os_str().to_owned(),
                    os("+refs/heads/tyrion-result:refs/heads/tyrion-candidate"),
                ],
            )?;
            if previous_revision == candidate.base_revision {
                git_checked(
                    Some(&repository),
                    &[os("merge"), os("--ff-only"), os("tyrion-candidate")],
                )?;
            } else {
                for commit in &candidate.candidate_commits {
                    let cherry_pick = git_output(
                        Some(&repository),
                        &[os("cherry-pick"), os("--no-edit"), os(commit)],
                    )?;
                    if !cherry_pick.status.success() {
                        let _ = git_output(Some(&repository), &[os("cherry-pick"), os("--abort")]);
                        return Err(TyrionError::IntegrationFailure {
                            kind: IntegrationFailureKind::Conflict,
                            message: format!(
                                "candidate commit {commit} conflicts with authoritative revision {previous_revision}: {}",
                                String::from_utf8_lossy(&cherry_pick.stderr).trim()
                            ),
                        });
                    }
                }
            }
        }
        let integrated_revision = git_text(&repository, &[os("rev-parse"), os("HEAD")])?
            .trim()
            .to_owned();
        git_checked(
            Some(&repository),
            &[os("branch"), os("-f"), os("tyrion-integration"), os("HEAD")],
        )?;
        let integrated_bundle = candidate
            .candidate_bundle
            .parent()
            .expect("candidate bundle has parent")
            .join("integrated.bundle");
        git_checked(
            Some(&repository),
            &[
                os("bundle"),
                os("create"),
                integrated_bundle.as_os_str().to_owned(),
                os("refs/heads/tyrion-integration"),
            ],
        )?;
        git_checked(
            Some(&repository),
            &[
                os("bundle"),
                os("verify"),
                integrated_bundle.as_os_str().to_owned(),
            ],
        )?;
        enforce_storage_ceiling(
            &[
                &candidate.base_bundle,
                &candidate.candidate_bundle,
                &integrated_bundle,
            ],
            assignment.max_storage_bytes,
        )?;
        let integrated_artifact = artifact("integrated_git_bundle", &integrated_bundle)?;
        Ok(GitIntegrated {
            integrated_revision,
            artifacts: vec![integrated_artifact],
            state: GitIntegratedState {
                integrated_bundle,
                integration_repository: repository,
                _integration_lock: integration_lock,
                previous_revision,
            },
        })
    }

    pub(super) fn verify_integrated(
        &self,
        assignment: &AssignmentContext,
        integrated: &GitIntegratedState,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        let revision = bundle_head(
            &integrated.integrated_bundle,
            "refs/heads/tyrion-integration",
        )?;
        self.verify_bundles(
            assignment,
            VerificationScope::Integrated,
            None,
            &integrated.integrated_bundle,
            &revision,
        )
    }

    pub(super) fn rollback_integration(
        &self,
        integrated: &GitIntegratedState,
    ) -> Result<(), TyrionError> {
        git_checked(
            Some(&integrated.integration_repository),
            &[os("reset"), os("--hard"), os(&integrated.previous_revision)],
        )?;
        git_checked(
            Some(&integrated.integration_repository),
            &[
                os("branch"),
                os("-f"),
                os("tyrion-integration"),
                os(&integrated.previous_revision),
            ],
        )?;
        Ok(())
    }

    fn verify_bundles(
        &self,
        assignment: &AssignmentContext,
        scope: VerificationScope,
        base: Option<(&Path, &str)>,
        result_bundle: &Path,
        revision: &str,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let crate::protocol::ExecutionSpec::CodexGit { repository, .. } = &assignment.execution
        else {
            unreachable!("Git verification has a codex_git execution spec")
        };
        let verification_runs = assignment
            .criteria
            .iter()
            .filter(|criterion| {
                criterion.verifier_type == crate::protocol::VerifierType::Deterministic
            })
            .map(|criterion| criterion.verification_depth.required_passes())
            .max()
            .unwrap_or(0);
        let mut records = Vec::with_capacity(assignment.criteria.len() * verification_runs);
        for run_index in 0..verification_runs {
            let run_attempt_id =
                format!("{}-verification-{}", assignment.attempt_id, run_index + 1);
            let sandbox_name = sandbox_name(scope.as_str(), &run_attempt_id);
            let sandbox = Sandbox::create(
                self,
                &sandbox_name,
                &self.config.policy_path,
                &self.config.openshell_provider,
                assignment.lease_expires_at,
            )?;
            sandbox.preflight(
                Path::new(repository),
                &self.data_dir,
                assignment.lease_expires_at,
            )?;
            if let Some((base_bundle, _)) = base {
                sandbox.upload(
                    base_bundle,
                    "/sandbox/base.bundle",
                    assignment.lease_expires_at,
                )?;
            }
            sandbox.upload(
                result_bundle,
                "/sandbox/result.bundle",
                assignment.lease_expires_at,
            )?;
            let setup = if let Some((_, base_revision)) = base {
                format!(
                    "set -eu; root=${{TYRION_WORKSPACE_ROOT:-/sandbox}}; git clone -q \"$root/base.bundle\" \"$root/repository\"; git -C \"$root/repository\" fetch -q \"$root/result.bundle\" refs/heads/tyrion-result:refs/heads/tyrion-result; git -C \"$root/repository\" checkout -q --detach {}; git -C \"$root/repository\" checkout -q --detach {}; git -C \"$root/repository\" fsck --full",
                    shell_quote(base_revision),
                    shell_quote(revision)
                )
            } else {
                format!(
                    "set -eu; root=${{TYRION_WORKSPACE_ROOT:-/sandbox}}; git clone -q \"$root/result.bundle\" \"$root/repository\"; git -C \"$root/repository\" checkout -q --detach {}; git -C \"$root/repository\" fsck --full",
                    shell_quote(revision)
                )
            };
            sandbox.exec_checked(&["sh", "-c", &setup], None, assignment.lease_expires_at)?;

            for criterion in assignment.criteria.iter().filter(|criterion| {
                criterion.verifier_type == crate::protocol::VerifierType::Deterministic
                    && run_index < criterion.verification_depth.required_passes()
            }) {
                let mut record =
                    sandbox.verify_command(criterion, scope, assignment.lease_expires_at)?;
                record.verifier_identity = format!("contained-command-{}", run_index + 1);
                records.push(record);
            }
            sandbox.delete()?;
        }
        Ok(records)
    }
}

struct Sandbox<'a> {
    runtime: &'a ContainedCodexRuntime,
    name: String,
    deleted: bool,
}

impl<'a> Sandbox<'a> {
    fn create(
        runtime: &'a ContainedCodexRuntime,
        name: &str,
        policy_path: &Path,
        provider: &str,
        deadline: i64,
    ) -> Result<Self, TyrionError> {
        let mut arguments = vec![
            "sandbox",
            "create",
            "--name",
            name,
            "--from",
            &runtime.config.base_image,
            "--policy",
            path_text(policy_path)?,
        ];
        arguments.extend(["--provider", provider]);
        arguments.extend([
            "--no-auto-providers",
            "--cpu",
            "2",
            "--memory",
            "2Gi",
            "--no-tty",
            "--",
            "true",
        ]);
        if let Err(error) = runtime.openshell_checked(&arguments, deadline) {
            let _ = runtime.delete_sandbox(name);
            return Err(error);
        }
        Ok(Self {
            runtime,
            name: name.to_owned(),
            deleted: false,
        })
    }

    fn upload(&self, local: &Path, remote: &str, deadline: i64) -> Result<(), TyrionError> {
        self.runtime.openshell_checked(
            &["sandbox", "upload", &self.name, path_text(local)?, remote],
            deadline,
        )?;
        Ok(())
    }

    fn download(&self, remote: &str, local: &Path, deadline: i64) -> Result<(), TyrionError> {
        self.runtime.openshell_checked(
            &["sandbox", "download", &self.name, remote, path_text(local)?],
            deadline,
        )?;
        Ok(())
    }

    fn exec_checked(
        &self,
        argv: &[&str],
        workdir: Option<&str>,
        deadline: i64,
    ) -> Result<Output, TyrionError> {
        let output = self.exec(argv, workdir, deadline)?;
        require_success("OpenShell sandbox command", output)
    }

    fn exec(
        &self,
        argv: &[&str],
        workdir: Option<&str>,
        deadline: i64,
    ) -> Result<Output, TyrionError> {
        let mut arguments = vec!["sandbox", "exec", "-n", &self.name];
        if let Some(workdir) = workdir {
            arguments.extend(["--workdir", workdir]);
        }
        arguments.extend(["--no-tty", "--"]);
        arguments.extend(argv.iter().copied());
        self.runtime.openshell(&arguments, deadline)
    }

    fn preflight(
        &self,
        repository: &Path,
        data_dir: &Path,
        deadline: i64,
    ) -> Result<(), TyrionError> {
        let host_repository = shell_quote(path_text(repository)?);
        let host_repository_parent = repository.parent().ok_or_else(|| {
            TyrionError::InvalidRequest("repository must have a parent directory".into())
        })?;
        let host_repository_parent = shell_quote(path_text(host_repository_parent)?);
        let host_state = shell_quote(path_text(data_dir)?);
        let probe = format!(
            "set -eu; printf tyrion-containment-probe; test \"$(cat /sys/fs/cgroup/pids.max)\" = 256; test \"$(getconf _NPROCESSORS_ONLN)\" = 2; memory_kib=$(awk '/MemTotal/ {{print $2}}' /proc/meminfo); test \"$memory_kib\" -ge 1900000; test \"$memory_kib\" -le 2097152; storage_kib=$(df -Pk /sandbox | awk 'NR==2 {{print $2}}'); test \"$storage_kib\" -le 4194304; test ! -e {host_repository}; test ! -e {host_repository_parent}; test ! -e {host_state}; test ! -e /var/run/docker.sock; test ! -e /run/containerd/containerd.sock; test ! -e /home/sandbox/.ssh; test ! -e /home/sandbox/.aws; test ! -e /home/sandbox/.config/gh; test ! -e /home/sandbox/.codex; test ! -e /home/sandbox/.claude; test ! -e /home/sandbox/.pi; test -z \"${{OPENAI_API_KEY:-}}${{ANTHROPIC_API_KEY:-}}${{GEMINI_API_KEY:-}}${{XAI_API_KEY:-}}${{GROQ_API_KEY:-}}${{OPENROUTER_API_KEY:-}}${{AWS_ACCESS_KEY_ID:-}}${{GH_TOKEN:-}}${{GITHUB_TOKEN:-}}${{SSH_AUTH_SOCK:-}}\"; test ! -r /opt/openshell/auth/sandbox.jwt; test ! -r /opt/openshell/tls/tls.key; if printf denied >/etc/tyrion-probe 2>/dev/null; then exit 91; fi; printf allowed >/sandbox/tyrion-probe; command -v curl >/dev/null; if curl -fsS --max-time 5 https://example.com >/dev/null 2>&1; then exit 92; fi; sleep 600 >/dev/null 2>&1 & descendant=$!; kill -0 \"$descendant\"; printf descendant-live"
        );
        self.exec_checked(&["sh", "-c", &probe], None, deadline)?;
        let logs = self
            .runtime
            .openshell(&["logs", &self.name, "-n", "300"], deadline)?;
        let logs = require_success("OpenShell logs", logs)?;
        let logs = String::from_utf8_lossy(&logs.stdout);
        if !logs.contains("Landlock ruleset built")
            || logs.contains("runtime cgroup pids.max is unavailable")
        {
            return Err(TyrionError::InvalidRequest(
                "OpenShell did not attest the hard Landlock and process boundary".into(),
            ));
        }
        Ok(())
    }

    fn verify_command(
        &self,
        criterion: &CriterionDefinition,
        scope: VerificationScope,
        deadline: i64,
    ) -> Result<VerificationRecord, TyrionError> {
        let Verifier::Command { argv } = &criterion.verifier else {
            unreachable!("validated Git criterion uses a command verifier")
        };
        let availability = self.exec(
            &[
                "sh",
                "-c",
                "command -v -- \"$1\" >/dev/null",
                "tyrion-verifier-availability",
                &argv[0],
            ],
            None,
            deadline,
        )?;
        if !availability.status.success() {
            return Ok(VerificationRecord {
                criterion_id: criterion.id.clone(),
                evidence_type: criterion.required_evidence.clone(),
                verifier_type: criterion.verifier_type,
                verification_attempt_id: uuid::Uuid::new_v4().to_string(),
                verifier_identity: "contained-command".into(),
                verifier_configuration: criterion.verifier_configuration.clone(),
                verifier_kind: VerificationKind::Command,
                procedure: criterion.verifier.clone(),
                environment: criterion.verification_environment.clone(),
                scope,
                outcome: EvidenceOutcome::Uncertain,
                observed: format!("verifier executable unavailable: {}", argv[0]),
                expected: serde_json::to_string(argv)?,
                material_contradiction: false,
                defect: Some(crate::protocol::VerificationDefect::Environment),
                producer_attempt_id: None,
            });
        }
        let borrowed = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let output = self.exec(&borrowed, Some("/sandbox/repository"), deadline)?;
        let observed = format!(
            "exit={}; stdout={}; stderr={}",
            output.status.code().unwrap_or(-1),
            truncate(&String::from_utf8_lossy(&output.stdout), 4096),
            truncate(&String::from_utf8_lossy(&output.stderr), 4096)
        );
        Ok(VerificationRecord {
            criterion_id: criterion.id.clone(),
            evidence_type: criterion.required_evidence.clone(),
            verifier_type: criterion.verifier_type,
            verification_attempt_id: uuid::Uuid::new_v4().to_string(),
            verifier_identity: "contained-command".into(),
            verifier_configuration: criterion.verifier_configuration.clone(),
            verifier_kind: VerificationKind::Command,
            procedure: criterion.verifier.clone(),
            environment: criterion.verification_environment.clone(),
            scope,
            outcome: if output.status.success() {
                EvidenceOutcome::Passed
            } else {
                EvidenceOutcome::Failed
            },
            observed,
            expected: serde_json::to_string(argv)?,
            material_contradiction: false,
            defect: (!output.status.success())
                .then_some(crate::protocol::VerificationDefect::Result),
            producer_attempt_id: None,
        })
    }

    fn delete(mut self) -> Result<(), TyrionError> {
        self.runtime.delete_sandbox(&self.name)?;
        self.deleted = true;
        Ok(())
    }
}

impl Drop for Sandbox<'_> {
    fn drop(&mut self) {
        if !self.deleted {
            let _ = self.runtime.delete_sandbox(&self.name);
        }
    }
}

impl ContainedCodexRuntime {
    fn openshell(&self, arguments: &[&str], deadline: i64) -> Result<Output, TyrionError> {
        ensure_lease_active(deadline)?;
        let mut command = Command::new(&self.config.openshell_binary);
        command
            .args(arguments)
            .env_clear()
            .env("XDG_CONFIG_HOME", &self.config.openshell_config_home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_until(command, deadline)
    }

    fn openshell_checked(&self, arguments: &[&str], deadline: i64) -> Result<Output, TyrionError> {
        let output = self.openshell(arguments, deadline)?;
        require_success("OpenShell", output)
    }

    fn delete_sandbox(&self, name: &str) -> Result<(), TyrionError> {
        let deadline = unix_timestamp()?.saturating_add(15);
        self.openshell_checked(&["sandbox", "delete", name], deadline)?;
        Ok(())
    }
}

fn validate_config(config: &RuntimeConfig) -> Result<(), TyrionError> {
    if config.openshell_version != "openshell 0.0.104"
        || config.codex_version != CODEX_VERSION
        || config.source_revision != SOURCE_REVISION
        || config.source_patch_sha256 != SOURCE_PATCH_SHA256
        || config.base_image != BASE_IMAGE
        || config.policy_sha256 != POLICY_SHA256
    {
        return Err(TyrionError::InvalidRequest(
            "Codex Worker configuration does not match the pinned repaired OpenShell profile"
                .into(),
        ));
    }
    if config.vcpus != 2
        || config.memory_mib != 2048
        || config.overlay_disk_mib != 4096
        || config.max_processes != 256
    {
        return Err(TyrionError::InvalidRequest(
            "Codex Worker configuration must use 2 vCPUs, 2048 MiB memory, 4096 MiB overlay, and 256 processes".into(),
        ));
    }
    if config.lease_ttl_seconds == 0 || config.lease_ttl_seconds > 3600 {
        return Err(TyrionError::InvalidRequest(
            "Worker Lease TTL must be between 1 and 3600 seconds".into(),
        ));
    }
    if config.model.trim().is_empty() || config.runtime_artifacts.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "Codex model and pinned runtime artifacts are required".into(),
        ));
    }
    if config.openshell_provider.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "OpenShell provider name must not be empty".into(),
        ));
    }
    verify_hash(&config.openshell_binary, &config.openshell_sha256)?;
    verify_hash(&config.policy_path, &config.policy_sha256)?;
    verify_hash(&config.gateway_config_path, &config.gateway_config_sha256)?;
    verify_hash(&config.kernel_config_path, &config.kernel_config_sha256)?;
    verify_hash(&config.source_patch_path, &config.source_patch_sha256)?;
    verify_hash(&config.codex_binary, &config.codex_sha256)?;
    if let Some(claude) = &config.claude {
        if claude.policy_sha256 != CLAUDE_POLICY_SHA256
            || claude.openshell_provider.trim().is_empty()
            || claude.version.trim().is_empty()
        {
            return Err(TyrionError::InvalidRequest(
                "Claude runtime does not match the pinned OpenShell profile".into(),
            ));
        }
        verify_hash(&claude.policy_path, &claude.policy_sha256)?;
        verify_hash(&claude.binary, &claude.sha256)?;
    }
    if let Some(pi) = &config.pi {
        if pi.policy_sha256 != PI_POLICY_SHA256
            || pi.openshell_provider.trim().is_empty()
            || pi.model_provider != "openai"
            || !pi.model.starts_with("openai/")
            || pi.model.len() == "openai/".len()
            || pi.version.trim().is_empty()
        {
            return Err(TyrionError::InvalidRequest(
                "Pi runtime does not match the pinned OpenShell profile".into(),
            ));
        }
        verify_hash(&pi.policy_path, &pi.policy_sha256)?;
        verify_hash(&pi.binary, &pi.sha256)?;
    }
    for artifact in &config.runtime_artifacts {
        verify_hash(&artifact.path, &artifact.sha256)?;
    }
    let gateway = fs::read_to_string(&config.gateway_config_path)?;
    for required in [
        "compute_drivers = [\"vm\"]",
        "enabled = true",
        "vcpus = 2",
        "mem_mib = 2048",
        "overlay_disk_mib = 4096",
    ] {
        if !gateway.lines().any(|line| line.trim() == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "gateway configuration is missing {required}"
            )));
        }
    }
    let kernel = fs::read_to_string(&config.kernel_config_path)?;
    validate_kernel_config(&kernel)?;
    let openshell = Command::new(&config.openshell_binary)
        .arg("--version")
        .env_clear()
        .env("XDG_CONFIG_HOME", &config.openshell_config_home)
        .output()?;
    let openshell = require_success("OpenShell version probe", openshell)?;
    if String::from_utf8_lossy(&openshell.stdout).trim() != config.openshell_version {
        return Err(TyrionError::InvalidRequest(
            "OpenShell binary version does not match its pin".into(),
        ));
    }
    Ok(())
}

fn validate_kernel_config(kernel: &str) -> Result<(), TyrionError> {
    for required in [
        "CONFIG_SECURITY=y",
        "CONFIG_SECURITY_LANDLOCK=y",
        "CONFIG_LSM=\"landlock,lockdown,yama,integrity\"",
        "CONFIG_CGROUP_PIDS=y",
        "CONFIG_SECCOMP_FILTER=y",
    ] {
        if !kernel.lines().any(|line| line == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "kernel configuration is missing {required}"
            )));
        }
    }
    Ok(())
}

fn create_base_bundle(repository: &Path, base: &str, bundle: &Path) -> Result<(), TyrionError> {
    let staging = bundle
        .parent()
        .expect("bundle path has parent")
        .join("base-staging.git");
    git_checked(
        None,
        &[
            os("clone"),
            os("--bare"),
            os("--no-hardlinks"),
            os("--quiet"),
            repository.as_os_str().to_owned(),
            staging.as_os_str().to_owned(),
        ],
    )?;
    let copied_base = git_text(
        &staging,
        &[os("rev-parse"), os(&format!("{base}^{{commit}}"))],
    )?;
    if copied_base.trim() != base {
        return Err(TyrionError::InvalidRequest(format!(
            "repository base resolved to {}, expected {base}",
            copied_base.trim()
        )));
    }
    let refs = git_text(&staging, &[os("for-each-ref"), os("--format=%(refname)")])?;
    for reference in refs.lines().filter(|value| !value.is_empty()) {
        git_checked(Some(&staging), &[os("update-ref"), os("-d"), os(reference)])?;
    }
    git_checked(
        Some(&staging),
        &[os("update-ref"), os("refs/heads/tyrion-base"), os(base)],
    )?;
    git_checked(
        Some(&staging),
        &[
            os("bundle"),
            os("create"),
            bundle.as_os_str().to_owned(),
            os("refs/heads/tyrion-base"),
        ],
    )?;
    git_checked(
        Some(&staging),
        &[os("bundle"), os("verify"), bundle.as_os_str().to_owned()],
    )?;
    fs::remove_dir_all(staging)?;
    Ok(())
}

struct ValidatedCandidate {
    candidate_revision: String,
    commits: Vec<String>,
    changed_paths: Vec<String>,
}

fn validate_candidate_bundle(
    artifact_dir: &Path,
    base_bundle: &Path,
    candidate_bundle: &Path,
    base_revision: &str,
    authorized_paths: &[String],
    allow_empty_changes: bool,
) -> Result<ValidatedCandidate, TyrionError> {
    let quarantine = artifact_dir.join("quarantine");
    git_checked(
        None,
        &[
            os("clone"),
            os("--quiet"),
            base_bundle.as_os_str().to_owned(),
            quarantine.as_os_str().to_owned(),
        ],
    )?;
    git_checked(
        Some(&quarantine),
        &[
            os("bundle"),
            os("verify"),
            candidate_bundle.as_os_str().to_owned(),
        ],
    )?;
    git_checked(
        Some(&quarantine),
        &[
            os("fetch"),
            os("--quiet"),
            candidate_bundle.as_os_str().to_owned(),
            os("refs/heads/tyrion-result:refs/heads/tyrion-result"),
        ],
    )?;
    let candidate_revision = git_text(
        &quarantine,
        &[os("rev-parse"), os("refs/heads/tyrion-result")],
    )?
    .trim()
    .to_owned();
    git_checked(
        Some(&quarantine),
        &[
            os("merge-base"),
            os("--is-ancestor"),
            os(base_revision),
            os(&candidate_revision),
        ],
    )?;
    let commit_text = git_text(
        &quarantine,
        &[
            os("rev-list"),
            os("--parents"),
            os("--reverse"),
            os("--topo-order"),
            os(&format!("{base_revision}..{candidate_revision}")),
        ],
    )?;
    let mut commits = Vec::new();
    let mut changed_paths = Vec::new();
    let mut previous_revision = base_revision;
    for line in commit_text.lines().filter(|line| !line.is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[1] != previous_revision {
            return Err(TyrionError::InvalidRequest(
                "Codex Result candidate history must be a linear chain from base_revision".into(),
            ));
        }
        let commit = fields[0];
        let changed = git_bytes(
            &quarantine,
            &[
                os("diff"),
                os("--name-only"),
                os("-z"),
                os(previous_revision),
                os(commit),
            ],
        )?;
        for path in changed
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
        {
            let path = String::from_utf8(path.to_vec()).map_err(|_| {
                TyrionError::InvalidRequest("Result contains a non-UTF-8 changed path".into())
            })?;
            if !changed_paths.contains(&path) {
                changed_paths.push(path);
            }
        }
        commits.push(commit.to_owned());
        previous_revision = commit;
    }
    if commits.is_empty() {
        return Err(TyrionError::InvalidRequest(
            "Codex Result contains no candidate commits".into(),
        ));
    }
    if changed_paths.is_empty() && !allow_empty_changes {
        return Err(TyrionError::InvalidRequest(
            "Codex Result contains no changed paths".into(),
        ));
    }
    for changed_path in &changed_paths {
        validate_relative_path(changed_path)?;
        if !authorized_paths.iter().any(|allowed| {
            changed_path == allowed
                || changed_path
                    .strip_prefix(allowed)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(TyrionError::InvalidRequest(format!(
                "Codex Result changed unauthorized path {changed_path}"
            )));
        }
    }
    fs::remove_dir_all(quarantine)?;
    Ok(ValidatedCandidate {
        candidate_revision,
        commits,
        changed_paths,
    })
}

fn attempt_script(base_revision: &str, model: &str, allow_empty_changes: bool) -> String {
    let auth_setup = r#": "${CODEX_AUTH_ACCESS_TOKEN:?missing brokered Codex access placeholder}"
: "${CODEX_AUTH_REFRESH_TOKEN:?missing brokered Codex refresh placeholder}"
: "${CODEX_AUTH_ACCOUNT_ID:?missing brokered Codex account placeholder}"
: "${CODEX_AUTH_ID_TOKEN:?missing brokered Codex identity placeholder}"
for value in "$CODEX_AUTH_ACCESS_TOKEN" "$CODEX_AUTH_REFRESH_TOKEN" "$CODEX_AUTH_ACCOUNT_ID" "$CODEX_AUTH_ID_TOKEN"; do
  case "$value" in
    openshell:resolve:env:*) ;;
    *) echo 'OpenShell exposed a raw Codex credential' >&2; exit 42 ;;
  esac
done
mkdir -p "$root/home/.codex"
last_refresh=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
cat >"$root/home/.codex/auth.json" <<AUTH
{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {
    "id_token": "e30.e30.placeholder",
    "access_token": "$CODEX_AUTH_ACCESS_TOKEN",
    "refresh_token": "$CODEX_AUTH_REFRESH_TOKEN",
    "account_id": "$CODEX_AUTH_ACCOUNT_ID"
  },
  "last_refresh": "$last_refresh"
}
AUTH
chmod 600 "$root/home/.codex/auth.json"
codex_auth_env="CODEX_AUTH_ACCESS_TOKEN=$CODEX_AUTH_ACCESS_TOKEN CODEX_AUTH_REFRESH_TOKEN=$CODEX_AUTH_REFRESH_TOKEN CODEX_AUTH_ACCOUNT_ID=$CODEX_AUTH_ACCOUNT_ID CODEX_AUTH_ID_TOKEN=$CODEX_AUTH_ID_TOKEN"
"#;
    let empty_change_action = if allow_empty_changes {
        "git -C \"$root/repository\" commit --allow-empty -qm 'test: record read-only assignment'"
    } else {
        "echo 'Codex produced no changes' >&2\n  exit 41"
    };
    format!(
        r#"#!/bin/sh
set -eu
root=${{TYRION_WORKSPACE_ROOT:-/sandbox}}
mkdir -p "$root/home"
chmod 700 "$root/home"
git clone -q "$root/base.bundle" "$root/repository"
git -C "$root/repository" checkout -q --detach {base}
{auth_setup}
env -i PATH=/usr/local/bin:/usr/bin:/bin HOME="$root/home" CODEX_HOME="$root/home/.codex" $codex_auth_env \
  ALL_PROXY="${{ALL_PROXY:-}}" HTTP_PROXY="${{HTTP_PROXY:-}}" HTTPS_PROXY="${{HTTPS_PROXY:-}}" NO_PROXY="${{NO_PROXY:-}}" \
  http_proxy="${{http_proxy:-}}" https_proxy="${{https_proxy:-}}" no_proxy="${{no_proxy:-}}" grpc_proxy="${{grpc_proxy:-}}" \
  SSL_CERT_FILE="${{SSL_CERT_FILE:-}}" CURL_CA_BUNDLE="${{CURL_CA_BUNDLE:-}}" GIT_SSL_CAINFO="${{GIT_SSL_CAINFO:-}}" \
  REQUESTS_CA_BUNDLE="${{REQUESTS_CA_BUNDLE:-}}" NODE_EXTRA_CA_CERTS="${{NODE_EXTRA_CA_CERTS:-}}" \
  NODE_USE_ENV_PROXY="${{NODE_USE_ENV_PROXY:-}}" DENO_CERT="${{DENO_CERT:-}}" \
  "$root/codex" exec --json --ephemeral --ignore-user-config \
  --dangerously-bypass-approvals-and-sandbox -C "$root/repository" \
  --model {model} --output-schema "$root/result-schema.json" \
  --output-last-message "$root/codex-result.json" - \
  <"$root/prompt.txt" >"$root/codex-events.jsonl"
git -C "$root/repository" add -A
if git -C "$root/repository" diff --cached --quiet; then
  {empty_change_action}
else
  env -i PATH=/usr/local/bin:/usr/bin:/bin \
    GIT_AUTHOR_NAME=Tyrion GIT_AUTHOR_EMAIL=worker@tyrion.invalid \
    GIT_COMMITTER_NAME=Tyrion GIT_COMMITTER_EMAIL=worker@tyrion.invalid \
    git -C "$root/repository" commit -qm 'feat: implement assignment'
fi
git -C "$root/repository" branch -f tyrion-result HEAD
git -C "$root/repository" bundle create "$root/candidate.bundle" \
  refs/heads/tyrion-result ^{base}
sync
"#,
        base = shell_quote(base_revision),
        model = shell_quote(model),
        auth_setup = auth_setup,
        empty_change_action = empty_change_action,
    )
}

fn worker_prompt(assignment: &AssignmentContext, base_revision: &str) -> String {
    let comparison_candidates = assignment
        .comparison_candidates
        .iter()
        .enumerate()
        .map(|(index, contender)| {
            serde_json::json!({
                "result_id": contender.result_id,
                "artifact_revision": contender.artifact_revision,
                "summary": contender.summary,
                "changed_paths": contender.changed_paths,
                "verification_outcomes": contender.verification_outcomes,
                "bundle": format!("/sandbox/contenders/{index}.bundle"),
            })
        })
        .collect::<Vec<_>>();
    format!(
        "Implement this Assignment in the current Git repository.\n\nGoal: {}\n\nMandate revision: {}\nPlan revision: {}\nImmutable base: {}\nAllowed changed paths: {}\nCompeting candidate bundles and Evidence: {}\n\nWhen candidate bundles are present, inspect each Git bundle and apply the declared comparison rule before editing. Do not use credentials or perform external effects. The authorized repository edit is the Result artifact, not an external effect, so known_effects must be an empty array. Return only the required structured final response.",
        assignment.goal,
        assignment.mandate_revision,
        assignment.plan_revision,
        base_revision,
        assignment.declared_write_scopes.join(", "),
        serde_json::to_string(&comparison_candidates)
            .expect("comparison candidate metadata is serializable"),
    )
}

fn result_schema() -> &'static [u8] {
    br#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "summary": {"type": "string"},
    "known_effects": {
      "type": "array",
      "description": "External effects beyond the local Result artifact; this contained slice permits none.",
      "items": {"type": "string"},
      "maxItems": 0
    }
  },
  "required": ["summary", "known_effects"]
}
"#
}

fn bundle_head(bundle: &Path, reference: &str) -> Result<String, TyrionError> {
    let output = git_text(
        bundle.parent().expect("bundle has parent"),
        &[
            os("bundle"),
            os("list-heads"),
            bundle.as_os_str().to_owned(),
            os(reference),
        ],
    )?;
    output
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| TyrionError::InvalidRequest("integrated bundle has no head".into()))
}

fn artifact(kind: &str, path: &Path) -> Result<ArtifactRecord, TyrionError> {
    Ok(ArtifactRecord {
        kind: kind.to_owned(),
        sha256: sha256_file(path)?,
        size_bytes: fs::metadata(path)?.len(),
        path: path_text(path)?.to_owned(),
    })
}

fn enforce_storage_ceiling(paths: &[&Path], ceiling: u64) -> Result<(), TyrionError> {
    let used = paths.iter().try_fold(0_u64, |total, path| {
        Ok::<_, std::io::Error>(total.saturating_add(fs::metadata(path)?.len()))
    })?;
    if used > ceiling {
        return Err(TyrionError::StorageCeilingExceeded {
            required_bytes: used,
            ceiling_bytes: ceiling,
        });
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), TyrionError> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), TyrionError> {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(TyrionError::InvalidRequest(
            "Result changed path must be a normalized relative path".into(),
        ));
    }
    Ok(())
}

fn verify_hash(path: &Path, expected: &str) -> Result<(), TyrionError> {
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(TyrionError::InvalidRequest(format!(
            "artifact {} has sha256 {actual}, expected {expected}",
            path.display()
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, TyrionError> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn git_checked(directory: Option<&Path>, arguments: &[OsString]) -> Result<Output, TyrionError> {
    let output = git_output(directory, arguments)?;
    require_success("Git", output)
}

fn git_text(directory: &Path, arguments: &[OsString]) -> Result<String, TyrionError> {
    let output = git_checked(Some(directory), arguments)?;
    String::from_utf8(output.stdout)
        .map_err(|_| TyrionError::InvalidRequest("Git returned non-UTF-8 text".into()))
}

fn git_bytes(directory: &Path, arguments: &[OsString]) -> Result<Vec<u8>, TyrionError> {
    Ok(git_checked(Some(directory), arguments)?.stdout)
}

fn git_output(directory: Option<&Path>, arguments: &[OsString]) -> Result<Output, TyrionError> {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    command
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command.output()?)
}

fn run_until(mut command: Command, deadline: i64) -> Result<Output, TyrionError> {
    ensure_lease_active(deadline)?;
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        TyrionError::InvalidRequest("contained command stdout was not piped".into())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        TyrionError::InvalidRequest("contained command stderr was not piped".into())
    })?;
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Output {
                status,
                stdout: join_reader(stdout_reader)?,
                stderr: join_reader(stderr_reader)?,
            });
        }
        if unix_timestamp()? >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TyrionError::WorkerLeaseExpired {
                operation: "while a contained command was running",
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, TyrionError> {
    reader
        .join()
        .map_err(|_| TyrionError::InvalidRequest("contained output reader panicked".into()))?
        .map_err(TyrionError::Io)
}

fn require_success(label: &str, output: Output) -> Result<Output, TyrionError> {
    if output.status.success() {
        return Ok(output);
    }
    Err(command_failure(label, output.status, &output.stderr))
}

fn command_failure(label: &str, status: ExitStatus, stderr: &[u8]) -> TyrionError {
    TyrionError::InvalidRequest(format!(
        "{label} failed with status {}: {}",
        status.code().unwrap_or(-1),
        truncate(&String::from_utf8_lossy(stderr), 4096)
    ))
}

fn ensure_lease_active(deadline: i64) -> Result<(), TyrionError> {
    if unix_timestamp()? >= deadline {
        return Err(TyrionError::WorkerLeaseExpired {
            operation: "before the contained operation completed",
        });
    }
    Ok(())
}

fn unix_timestamp() -> Result<i64, TyrionError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| TyrionError::InvalidRequest(format!("system clock error: {error}")))?
        .as_secs() as i64)
}

fn sandbox_name(scope: &str, attempt_id: &str) -> String {
    let scope_code = match scope {
        "attempt" => "a",
        "candidate" => "c",
        "integrated" => "i",
        _ => "x",
    };
    let digest = format!("{:x}", Sha256::digest(format!("{scope}:{attempt_id}")));
    format!("tyrion-{scope_code}-{}", &digest[..10])
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_text(path: &Path) -> Result<&str, TyrionError> {
    path.to_str().ok_or_else(|| {
        TyrionError::InvalidRequest(format!("path {} is not valid UTF-8", path.display()))
    })
}

fn os(value: &str) -> OsString {
    OsString::from(value)
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::{attempt_script, result_schema, validate_kernel_config};

    #[test]
    fn repaired_kernel_activates_landlock_as_an_lsm() {
        let inactive = "CONFIG_SECURITY=y\nCONFIG_SECURITY_LANDLOCK=y\nCONFIG_CGROUP_PIDS=y\nCONFIG_SECCOMP_FILTER=y\n";
        assert!(validate_kernel_config(inactive).is_err());

        let active = "CONFIG_SECURITY=y\nCONFIG_SECURITY_LANDLOCK=y\nCONFIG_LSM=\"landlock,lockdown,yama,integrity\"\nCONFIG_CGROUP_PIDS=y\nCONFIG_SECCOMP_FILTER=y\n";
        assert!(validate_kernel_config(active).is_ok());
    }

    #[test]
    fn codex_receives_only_the_brokered_network_environment() {
        let script = attempt_script(
            "0123456789012345678901234567890123456789",
            "test-model",
            false,
        );
        for required in [
            "HTTP_PROXY=\"${HTTP_PROXY:-}\"",
            "HTTPS_PROXY=\"${HTTPS_PROXY:-}\"",
            "SSL_CERT_FILE=\"${SSL_CERT_FILE:-}\"",
            "CURL_CA_BUNDLE=\"${CURL_CA_BUNDLE:-}\"",
            "CODEX_AUTH_ACCESS_TOKEN=$CODEX_AUTH_ACCESS_TOKEN",
        ] {
            assert!(script.contains(required), "missing {required}");
        }
        assert!(!script.contains("AWS_ACCESS_KEY_ID="));
        assert!(!script.contains("GH_TOKEN="));
        assert!(!script.contains("SSH_AUTH_SOCK="));
    }

    #[test]
    fn codex_auth_file_is_locally_parseable_without_raw_credentials() {
        let script = attempt_script(
            "0123456789012345678901234567890123456789",
            "test-model",
            false,
        );
        let auth_file = script
            .split("cat >\"$root/home/.codex/auth.json\" <<AUTH\n")
            .nth(1)
            .and_then(|tail| tail.split("\nAUTH\n").next())
            .expect("attempt script contains the Codex auth file");

        assert!(auth_file.contains("\"id_token\": \"e30.e30.placeholder\""));
        assert!(auth_file.contains("\"last_refresh\": \"$last_refresh\""));
        assert!(auth_file.contains("\"access_token\": \"$CODEX_AUTH_ACCESS_TOKEN\""));
        assert!(auth_file.contains("\"account_id\": \"$CODEX_AUTH_ACCOUNT_ID\""));
        assert!(!auth_file.contains("openshell:resolve:env:CODEX_AUTH_ACCESS_TOKEN"));
        assert!(!auth_file.contains("openshell:resolve:env:CODEX_AUTH_ID_TOKEN"));
    }

    #[test]
    fn contained_codex_schema_forbids_external_effects() {
        let schema: serde_json::Value =
            serde_json::from_slice(result_schema()).expect("result schema is valid JSON");
        assert_eq!(schema["properties"]["known_effects"]["maxItems"], 0);
    }
}

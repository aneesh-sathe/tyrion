use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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
use crate::containment::RELAY_SOURCE;
use crate::domain::EvidenceOutcome;
use crate::error::IntegrationFailureKind;
use crate::protocol::Verifier;
use crate::TyrionError;

/// The Worker containment boundary is one disposable Docker container per
/// Attempt, verification run, and comparison. Every ceiling is set by the
/// Docker daemon from outside the container and is not raisable by guest
/// root, no host path is ever bind-mounted in, and the only writable mount is
/// the sized `/sandbox` tmpfs.
pub(super) const CONTAINMENT_PROFILE: &str = "docker-hardened-v1";
pub(crate) const CODEX_VERSION: &str = "codex-cli 0.156.1";
/// The single writable mount inside every sandbox.
const SANDBOX_ROOT: &str = "/sandbox";
/// Every container and network Tyrion creates carries its Attempt, so
/// cleanup never depends on reconstructing a name.
const ATTEMPT_LABEL: &str = "tyrion.attempt";
/// Deterministic `PATH` for the Docker CLI, which never inherits the
/// Principal environment.
const DOCKER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The vetted Docker runtime Tyrion contains Workers with. Tyrion never pulls
/// an image and never resolves an ambient Docker context: the operator
/// provisions the digest-pinned Worker image, and every field below is
/// checked before the daemon accepts the configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    docker_binary: PathBuf,
    docker_sha256: String,
    docker_version: String,
    docker_host: String,
    /// The Worker image, pinned by registry digest.
    worker_image: String,
    /// The locally resolved image identity that `worker_image` must launch.
    worker_image_id: String,
    codex_binary: PathBuf,
    codex_version: String,
    codex_sha256: String,
    /// Codex delegates file edits and shell work to a companion host binary
    /// and fails the Assignment without it. It is pinned and transferred
    /// exactly like the harness itself.
    #[serde(default)]
    codex_code_mode_host: Option<PinnedBinary>,
    model: String,
    /// Absent means every sandbox runs with no network at all.
    #[serde(default)]
    egress: Option<EgressConfig>,
    /// Environment variable names the Principal started `tyriond` with that
    /// may reach a Worker. Empty by default: credential availability on the
    /// host never implies permission to use it.
    #[serde(default)]
    worker_credentials: Vec<String>,
    /// A Codex subscription login on the host. Codex reads tokens from a file
    /// rather than the environment, so Tyrion reads this at dispatch and
    /// streams a minimal disposable copy into the sandbox. Absent means Codex
    /// Workers get no credential at all.
    #[serde(default)]
    codex_auth_file: Option<PathBuf>,
    #[serde(default)]
    claude: Option<ClaudeRuntimeConfig>,
    #[serde(default)]
    pi: Option<PiRuntimeConfig>,
    lease_ttl_seconds: u64,
    vcpus: u32,
    /// Bounds process memory and the writable tmpfs together, because tmpfs
    /// pages are charged to the container memory cgroup.
    memory_mib: u64,
    writable_storage_mib: u64,
    max_processes: u32,
}

/// The complete set of destinations a Worker may reach. Each one is brokered
/// by its own relay on a per-Attempt internal network; everything else is
/// unreachable because the network has no route off the bridge.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EgressConfig {
    destinations: Vec<EgressDestination>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct EgressDestination {
    host: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedBinary {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeRuntimeConfig {
    binary: PathBuf,
    version: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PiRuntimeConfig {
    model_provider: String,
    model: String,
    binary: PathBuf,
    version: String,
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
        let credentials = sandbox.runtime.credential_arguments();
        let mut arguments = vec!["exec", "--interactive", "--workdir", SANDBOX_ROOT];
        arguments.extend(credentials.iter().map(String::as_str));
        arguments.extend([
            sandbox.name.as_str(),
            "env",
            "PATH=/usr/local/bin:/usr/bin:/bin",
        ]);
        let mut command = sandbox.runtime.docker_command(&arguments);
        sandbox.runtime.apply_credentials(&mut command);
        command
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
            .arg("HOME=/sandbox")
            .arg("TMPDIR=/sandbox/tmp")
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
        let docker_config = data_dir.join("docker");
        create_private_dir(&docker_config)?;
        fs::write(docker_config.join("config.json"), b"{}")?;
        validate_worker_image(&config, data_dir)?;
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
            "docker_version".into(),
            serde_json::json!(self.config.docker_version),
        );
        settings.insert(
            "worker_image".into(),
            serde_json::json!(self.config.worker_image),
        );
        settings.insert(
            "worker_image_id".into(),
            serde_json::json!(self.config.worker_image_id),
        );
        settings.insert("vcpus".into(), serde_json::json!(self.config.vcpus));
        settings.insert(
            "memory_mib".into(),
            serde_json::json!(self.config.memory_mib),
        );
        settings.insert(
            "writable_storage_mib".into(),
            serde_json::json!(self.config.writable_storage_mib),
        );
        settings.insert(
            "max_processes".into(),
            serde_json::json!(self.config.max_processes),
        );
        settings.insert(
            "brokered_egress".into(),
            serde_json::json!(self
                .config
                .egress
                .as_ref()
                .map(|egress| egress
                    .destinations
                    .iter()
                    .map(|destination| format!("{}:{}", destination.host, destination.port))
                    .collect::<Vec<_>>())
                .unwrap_or_default()),
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
                .writable_storage_mib
                .saturating_mul(1024)
                .saturating_mul(1024),
            containment_profile: format!("{CONTAINMENT_PROFILE}-{}", &self.fingerprint[..16]),
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
            &assignment.attempt_id,
            NetworkPolicy::Brokered,
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
        if configuration.adapter.kind == super::routing::WorkerAdapterKind::CodexAppServer {
            if let Some(host) = self.config.codex_code_mode_host.as_ref() {
                sandbox.upload(
                    &host.path,
                    "/sandbox/codex-code-mode-host",
                    assignment.lease_expires_at,
                )?;
                sandbox.exec_checked(
                    &["chmod", "700", "/sandbox/codex-code-mode-host"],
                    None,
                    assignment.lease_expires_at,
                )?;
            }
            self.deliver_codex_login(&sandbox, assignment.lease_expires_at)?;
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

    /// Codex reads its subscription login from a file rather than the
    /// environment, so the equivalent of forwarding one named variable is to
    /// copy exactly the four token fields into a disposable guest home. Only
    /// those fields travel; the host file is never uploaded wholesale, never
    /// logged, and never reaches a command line.
    fn deliver_codex_login(&self, sandbox: &Sandbox<'_>, deadline: i64) -> Result<(), TyrionError> {
        let Some(path) = self.config.codex_auth_file.as_ref() else {
            return Ok(());
        };
        let host: Value = serde_json::from_slice(&fs::read(path)?).map_err(|_| {
            TyrionError::InvalidRequest("the Codex login file is not valid JSON".into())
        })?;
        let field = |name: &str| -> Result<String, TyrionError> {
            host["tokens"][name]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    TyrionError::InvalidRequest(format!(
                        "the Codex login file has no tokens.{name}; run `codex login` first"
                    ))
                })
        };
        let guest = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": Value::Null,
            "tokens": {
                "id_token": field("id_token")?,
                "access_token": field("access_token")?,
                "refresh_token": field("refresh_token")?,
                "account_id": field("account_id")?,
            },
            "last_refresh": host["last_refresh"].as_str().unwrap_or_default(),
        });
        sandbox.upload_bytes(
            &serde_json::to_vec(&guest)?,
            "/sandbox/.codex/auth.json",
            "600",
            deadline,
        )
    }

    fn structured_runtime_profile(
        &self,
        kind: super::routing::WorkerAdapterKind,
    ) -> Result<StructuredRuntimeProfile<'_>, TyrionError> {
        match kind {
            super::routing::WorkerAdapterKind::CodexAppServer => Ok(StructuredRuntimeProfile {
                binary: &self.config.codex_binary,
                remote_binary: "/sandbox/codex",
                binary_environment: "Codex",
                version: &self.config.codex_version,
            }),
            super::routing::WorkerAdapterKind::ClaudeAgentSdk => {
                let claude = self.config.claude.as_ref().ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "Claude Worker execution requires a pinned Claude runtime profile".into(),
                    )
                })?;
                Ok(StructuredRuntimeProfile {
                    binary: &claude.binary,
                    remote_binary: "/sandbox/claude",
                    binary_environment: "Claude",
                    version: &claude.version,
                })
            }
            super::routing::WorkerAdapterKind::PiRpc => {
                let pi = self.config.pi.as_ref().ok_or_else(|| {
                    TyrionError::InvalidRequest(
                        "Pi Worker execution requires a pinned Pi runtime profile".into(),
                    )
                })?;
                Ok(StructuredRuntimeProfile {
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

    /// Remove every container and network this Attempt ever created. The
    /// Attempt label is the authority, so a sandbox whose name is not
    /// reachable from the durable record is still cleaned up.
    pub(super) fn cleanup_stranded_attempt(&self, attempt_id: &str) -> Result<(), TyrionError> {
        let deadline = unix_timestamp()?.saturating_add(120);
        let filter = format!("label={ATTEMPT_LABEL}={attempt_id}");
        let containers =
            self.docker_checked(&["ps", "--all", "--quiet", "--filter", &filter], deadline)?;
        for container in text_lines(&containers.stdout) {
            self.delete_container(&container)?;
        }
        let networks =
            self.docker_checked(&["network", "ls", "--quiet", "--filter", &filter], deadline)?;
        for network in text_lines(&networks.stdout) {
            self.docker_checked(&["network", "rm", &network], deadline)?;
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
            &assignment.attempt_id,
            NetworkPolicy::Brokered,
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
                &self.config.worker_credentials,
            ),
        )?;
        fs::set_permissions(&attempt_script_path, fs::Permissions::from_mode(0o700))?;
        sandbox.upload(
            &attempt_script_path,
            "/sandbox/run-attempt.sh",
            assignment.lease_expires_at,
        )?;
        sandbox.exec_credentialed(
            &["sh", "/sandbox/run-attempt.sh"],
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
                &assignment.attempt_id,
                NetworkPolicy::Denied,
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

/// A single disposable Docker container. Creation, transfer, execution, and
/// deletion are the whole containment seam; everything above it is
/// runtime-independent.
struct Sandbox<'a> {
    runtime: &'a ContainedCodexRuntime,
    name: String,
    network: Option<AttemptNetwork<'a>>,
    deleted: bool,
}

/// Whether a sandbox may reach anything at all. Verification runs are always
/// denied; Worker Attempts reach only the configured brokered destinations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetworkPolicy {
    Denied,
    Brokered,
}

impl<'a> Sandbox<'a> {
    fn create(
        runtime: &'a ContainedCodexRuntime,
        name: &str,
        attempt_id: &str,
        policy: NetworkPolicy,
        deadline: i64,
    ) -> Result<Self, TyrionError> {
        let network = match (policy, runtime.config.egress.as_ref()) {
            (NetworkPolicy::Brokered, Some(egress)) => Some(AttemptNetwork::create(
                runtime, name, attempt_id, egress, deadline,
            )?),
            _ => None,
        };
        let mut sandbox = Self {
            runtime,
            name: name.to_owned(),
            network,
            deleted: false,
        };
        if let Err(error) = sandbox.start(attempt_id, deadline) {
            sandbox.discard();
            return Err(error);
        }
        Ok(sandbox)
    }

    fn start(&mut self, attempt_id: &str, deadline: i64) -> Result<(), TyrionError> {
        let config = &self.runtime.config;
        let label = format!("{ATTEMPT_LABEL}={attempt_id}");
        let memory = format!("{}m", config.memory_mib);
        let cpus = config.vcpus.to_string();
        let cpuset = format!("0-{}", config.vcpus.saturating_sub(1));
        let pids = config.max_processes.to_string();
        // Docker defaults a tmpfs to noexec, but /sandbox is the only writable
        // mount and holds every artifact Tyrion streams in, including the
        // harness binary. `nosuid` and `nodev` stay; the image already ships
        // interpreters, so noexec here bought no containment.
        let tmpfs = format!(
            "{SANDBOX_ROOT}:rw,exec,nosuid,nodev,size={}m,mode=1777",
            config.writable_storage_mib
        );
        let home = format!("HOME={SANDBOX_ROOT}");
        let tmpdir = format!("TMPDIR={SANDBOX_ROOT}/tmp");
        let xdg = format!("XDG_CONFIG_HOME={SANDBOX_ROOT}/.config");
        // Adapter dependencies live in the pinned image rather than being
        // transferred per Attempt, so they are covered by the image digest
        // Tyrion already verifies at launch.
        let workspace = format!("TYRION_WORKSPACE_ROOT={SANDBOX_ROOT}");
        // The container outlives no Worker Lease: it exits on its own when the
        // lease does, which bounds its lifetime even if Tyrion itself is lost.
        let lifetime = deadline
            .saturating_sub(unix_timestamp()?)
            .clamp(1, 86_400)
            .to_string();
        let network = match &self.network {
            Some(network) => network.internal.clone(),
            None => "none".to_owned(),
        };
        let mut arguments = vec![
            "run",
            "--detach",
            "--name",
            &self.name,
            "--label",
            &label,
            "--network",
            &network,
            "--read-only",
            "--pids-limit",
            &pids,
            "--memory",
            &memory,
            "--memory-swap",
            &memory,
            "--cpus",
            &cpus,
            "--cpuset-cpus",
            &cpuset,
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            // Docker Desktop leaves seccomp unconfined unless it is asked for.
            "--security-opt",
            "seccomp=builtin",
            "--user",
            "65534:65534",
            "--tmpfs",
            &tmpfs,
            "--workdir",
            SANDBOX_ROOT,
            "--env",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "--env",
            &home,
            "--env",
            &tmpdir,
            "--env",
            &xdg,
            "--env",
            &workspace,
            "--env",
            "PYTHONPATH=/opt/tyrion",
        ];
        let aliases = self
            .network
            .as_ref()
            .map(AttemptNetwork::host_aliases)
            .unwrap_or_default();
        for alias in &aliases {
            arguments.extend(["--add-host", alias]);
        }
        arguments.extend([config.worker_image.as_str(), "sleep", &lifetime]);
        self.runtime.docker_checked(&arguments, deadline)?;
        // The image that actually launched, not the one that was requested.
        let launched = self
            .runtime
            .docker_checked(&["inspect", "--format", "{{.Image}}", &self.name], deadline)?;
        let launched = String::from_utf8_lossy(&launched.stdout).trim().to_owned();
        if launched != config.worker_image_id {
            return Err(TyrionError::SecurityInvariantViolation(format!(
                "sandbox launched image {launched}, not the pinned Worker image {}",
                config.worker_image_id
            )));
        }
        Ok(())
    }

    /// Stream a host file in. `docker cp` is deliberately unused: on Docker
    /// Desktop it reports success while writing underneath a tmpfs mount
    /// instead of into it.
    fn upload(&self, local: &Path, remote: &str, deadline: i64) -> Result<(), TyrionError> {
        let source = File::open(local)?;
        let quoted = shell_quote(remote);
        let script = format!("set -eu; mkdir -p \"$(dirname {quoted})\"; cat > {quoted}");
        let mut command = self.runtime.docker_command(&[
            "exec",
            "--interactive",
            &self.name,
            "sh",
            "-c",
            &script,
        ]);
        command.stdin(Stdio::from(source));
        require_success("Docker sandbox upload", run_until(command, deadline)?)?;
        Ok(())
    }

    /// Stream bytes Tyrion holds in memory into the sandbox. Used for
    /// credential material, which must never touch host disk or a command
    /// line on its way in.
    fn upload_bytes(
        &self,
        bytes: &[u8],
        remote: &str,
        mode: &str,
        deadline: i64,
    ) -> Result<(), TyrionError> {
        let quoted = shell_quote(remote);
        let script = format!(
            "set -eu; mkdir -p \"$(dirname {quoted})\"; umask 077; cat > {quoted}; chmod {mode} {quoted}"
        );
        let mut command = self.runtime.docker_command(&[
            "exec",
            "--interactive",
            &self.name,
            "sh",
            "-c",
            &script,
        ]);
        command.stdin(Stdio::piped());
        ensure_lease_active(deadline)?;
        let mut child = command.spawn()?;
        {
            let mut input = child.stdin.take().ok_or_else(|| {
                TyrionError::InvalidRequest("sandbox upload has no input channel".into())
            })?;
            input.write_all(bytes)?;
            input.flush()?;
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(TyrionError::InvalidRequest(
                "Docker sandbox credential upload failed".into(),
            ));
        }
        Ok(())
    }

    fn download(&self, remote: &str, local: &Path, deadline: i64) -> Result<(), TyrionError> {
        let destination = File::create(local)?;
        let script = format!("set -eu; cat {}", shell_quote(remote));
        let mut command = self
            .runtime
            .docker_command(&["exec", &self.name, "sh", "-c", &script]);
        command.stdout(Stdio::from(destination));
        require_success("Docker sandbox download", run_until(command, deadline)?)?;
        Ok(())
    }

    fn exec_checked(
        &self,
        argv: &[&str],
        workdir: Option<&str>,
        deadline: i64,
    ) -> Result<Output, TyrionError> {
        let output = self.exec(argv, workdir, deadline)?;
        require_success("Docker sandbox command", output)
    }

    fn exec(
        &self,
        argv: &[&str],
        workdir: Option<&str>,
        deadline: i64,
    ) -> Result<Output, TyrionError> {
        let mut arguments = vec!["exec"];
        if let Some(workdir) = workdir {
            arguments.extend(["--workdir", workdir]);
        }
        arguments.push(&self.name);
        arguments.extend(argv.iter().copied());
        self.runtime.docker(&arguments, deadline)
    }

    /// The one execution that may carry a provider credential. It is scoped to
    /// this single command, never to the container.
    fn exec_credentialed(&self, argv: &[&str], deadline: i64) -> Result<Output, TyrionError> {
        ensure_lease_active(deadline)?;
        let credentials = self.runtime.credential_arguments();
        let mut arguments = vec!["exec"];
        arguments.extend(credentials.iter().map(String::as_str));
        arguments.push(&self.name);
        arguments.extend(argv.iter().copied());
        let mut command = self.runtime.docker_command(&arguments);
        self.runtime.apply_credentials(&mut command);
        require_success(
            "Docker credentialed sandbox command",
            run_until(command, deadline)?,
        )
    }

    /// Prove the boundary from inside it before any Worker code runs. Each
    /// ceiling is read from the container's own cgroup, which the Docker
    /// daemon set from outside and mounts read-only.
    fn preflight(
        &self,
        repository: &Path,
        data_dir: &Path,
        deadline: i64,
    ) -> Result<(), TyrionError> {
        let config = &self.runtime.config;
        let host_repository = shell_quote(path_text(repository)?);
        let host_repository_parent = repository.parent().ok_or_else(|| {
            TyrionError::InvalidRequest("repository must have a parent directory".into())
        })?;
        let host_repository_parent = shell_quote(path_text(host_repository_parent)?);
        let host_state = shell_quote(path_text(data_dir)?);
        let pids = config.max_processes;
        let cpu_quota = u64::from(config.vcpus).saturating_mul(100_000);
        let vcpus = config.vcpus;
        let memory_bytes = config.memory_mib.saturating_mul(1024).saturating_mul(1024);
        let storage_kib = config.writable_storage_mib.saturating_mul(1024);
        let probe = format!(
            "set -eu; \
             printf tyrion-containment-probe; \
             test \"$(cat /sys/fs/cgroup/pids.max)\" = {pids}; \
             test \"$(cat /sys/fs/cgroup/memory.max)\" = {memory_bytes}; \
             test \"$(cat /sys/fs/cgroup/memory.swap.max)\" = 0; \
             test \"$(cat /sys/fs/cgroup/cpu.max)\" = '{cpu_quota} 100000'; \
             test \"$(nproc)\" = {vcpus}; \
             test \"$(df -Pk {SANDBOX_ROOT} | awk 'NR==2 {{print $2}}')\" -le {storage_kib}; \
             if printf 512 >/sys/fs/cgroup/pids.max 2>/dev/null; then exit 90; fi; \
             if printf denied >/etc/tyrion-probe 2>/dev/null; then exit 91; fi; \
             printf allowed >{SANDBOX_ROOT}/tyrion-probe; \
             printf '#!/bin/sh\\necho executable' >{SANDBOX_ROOT}/tyrion-exec-probe; \
             chmod 700 {SANDBOX_ROOT}/tyrion-exec-probe; \
             test \"$({SANDBOX_ROOT}/tyrion-exec-probe)\" = executable; \
             test \"$(awk '/^CapEff:/ {{print $2}}' /proc/self/status)\" = 0000000000000000; \
             test \"$(awk '/^NoNewPrivs:/ {{print $2}}' /proc/self/status)\" = 1; \
             test \"$(awk '/^Seccomp:/ {{print $2}}' /proc/self/status)\" != 0; \
             test \"$(id -u)\" != 0; \
             test ! -e {host_repository}; \
             test ! -e {host_repository_parent}; \
             test ! -e {host_state}; \
             test ! -e /var/run/docker.sock; \
             test ! -e /run/containerd/containerd.sock; \
             test ! -e \"$HOME/.ssh\"; \
             test ! -e \"$HOME/.aws\"; \
             test ! -e \"$HOME/.config/gh\"; \
             test ! -e \"$HOME/.codex\"; \
             test ! -e \"$HOME/.claude\"; \
             test ! -e \"$HOME/.pi\"; \
             test -z \"${{OPENAI_API_KEY:-}}${{ANTHROPIC_API_KEY:-}}${{GEMINI_API_KEY:-}}${{XAI_API_KEY:-}}${{GROQ_API_KEY:-}}${{OPENROUTER_API_KEY:-}}${{AWS_ACCESS_KEY_ID:-}}${{GH_TOKEN:-}}${{GITHUB_TOKEN:-}}${{SSH_AUTH_SOCK:-}}\"; \
             if awk '$5 != \"/\" && $5 !~ /^\\/(proc|sys|dev|sandbox)/ && $5 !~ /^\\/etc\\/(hosts|hostname|resolv.conf)$/ {{ print }}' /proc/self/mountinfo | grep -q .; then exit 93; fi; \
             if curl -fsS --max-time 5 https://example.com >/dev/null 2>&1; then exit 92; fi; \
             sleep 600 >/dev/null 2>&1 & descendant=$!; \
             kill -0 \"$descendant\"; \
             printf descendant-live"
        );
        let output = self
            .exec(&["sh", "-c", &probe], None, deadline)
            .map_err(|error| TyrionError::SecurityInvariantViolation(error.to_string()))?;
        if !output.status.success() {
            return Err(TyrionError::SecurityInvariantViolation(format!(
                "Docker containment preflight failed with status {}: {}",
                output.status.code().unwrap_or(-1),
                truncate(&String::from_utf8_lossy(&output.stderr), 4096)
            )));
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
        let workdir = format!("{SANDBOX_ROOT}/repository");
        let output = self.exec(&borrowed, Some(&workdir), deadline)?;
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
        self.runtime.delete_container(&self.name)?;
        if let Some(network) = self.network.take() {
            network.delete()?;
        }
        self.deleted = true;
        Ok(())
    }

    /// Best-effort teardown for a sandbox that failed before it was usable.
    fn discard(&mut self) {
        if self.deleted {
            return;
        }
        let _ = self.runtime.delete_container(&self.name);
        if let Some(network) = self.network.take() {
            let _ = network.delete();
        }
        self.deleted = true;
    }
}

impl Drop for Sandbox<'_> {
    fn drop(&mut self) {
        self.discard();
    }
}

/// A per-Attempt internal bridge plus one destination-pinned relay for each
/// authorized destination. The bridge has no route off itself, so the relays
/// are the only way out and each reaches exactly one `host:port`.
struct AttemptNetwork<'a> {
    runtime: &'a ContainedCodexRuntime,
    internal: String,
    egress: String,
    relays: Vec<String>,
    aliases: Vec<String>,
}

impl<'a> AttemptNetwork<'a> {
    fn create(
        runtime: &'a ContainedCodexRuntime,
        name: &str,
        attempt_id: &str,
        egress: &EgressConfig,
        deadline: i64,
    ) -> Result<Self, TyrionError> {
        let label = format!("{ATTEMPT_LABEL}={attempt_id}");
        let mut network = Self {
            runtime,
            internal: format!("{name}-net"),
            egress: format!("{name}-out"),
            relays: Vec::new(),
            aliases: Vec::new(),
        };
        if let Err(error) = network.build(&label, egress, deadline) {
            let _ = network.remove();
            return Err(error);
        }
        Ok(network)
    }

    fn build(
        &mut self,
        label: &str,
        egress: &EgressConfig,
        deadline: i64,
    ) -> Result<(), TyrionError> {
        self.runtime.docker_checked(
            &[
                "network",
                "create",
                "--internal",
                "--label",
                label,
                &self.internal,
            ],
            deadline,
        )?;
        self.runtime.docker_checked(
            &["network", "create", "--label", label, &self.egress],
            deadline,
        )?;
        for (index, destination) in egress.destinations.iter().enumerate() {
            let relay = format!("{}-r{index}", self.internal);
            let port = destination.port.to_string();
            // The relay starts on the egress bridge so it can reach the
            // destination, and is only then joined to the Worker's bridge.
            self.runtime.docker_checked(
                &[
                    "run",
                    "--detach",
                    "--name",
                    &relay,
                    "--label",
                    label,
                    "--network",
                    &self.egress,
                    "--read-only",
                    "--pids-limit",
                    "64",
                    "--memory",
                    "128m",
                    "--memory-swap",
                    "128m",
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges",
                    "--security-opt",
                    "seccomp=builtin",
                    "--user",
                    "65534:65534",
                    &self.runtime.config.worker_image,
                    "python3",
                    "-c",
                    RELAY_SOURCE,
                    &destination.host,
                    &port,
                ],
                deadline,
            )?;
            self.relays.push(relay.clone());
            self.runtime
                .docker_checked(&["network", "connect", &self.internal, &relay], deadline)?;
            self.await_ready(&relay, deadline)?;
            let address = self.runtime.docker_checked(
                &[
                    "inspect",
                    "--format",
                    &format!(
                        "{{{{(index .NetworkSettings.Networks \"{}\").IPAddress}}}}",
                        self.internal
                    ),
                    &relay,
                ],
                deadline,
            )?;
            let address = String::from_utf8_lossy(&address.stdout).trim().to_owned();
            if address.is_empty() {
                return Err(TyrionError::SecurityInvariantViolation(
                    "brokered egress relay has no address on the Attempt network".into(),
                ));
            }
            self.aliases.push(format!("{}:{address}", destination.host));
        }
        Ok(())
    }

    fn await_ready(&self, relay: &str, deadline: i64) -> Result<(), TyrionError> {
        loop {
            let logs = self
                .runtime
                .docker_checked(&["logs", relay], deadline.min(unix_timestamp()? + 60))?;
            if String::from_utf8_lossy(&logs.stderr).contains("tyrion-relay-ready")
                || String::from_utf8_lossy(&logs.stdout).contains("tyrion-relay-ready")
            {
                return Ok(());
            }
            if unix_timestamp()? >= deadline {
                return Err(TyrionError::WorkerLeaseExpired {
                    operation: "while a brokered egress relay was starting",
                });
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn host_aliases(&self) -> Vec<&str> {
        self.aliases.iter().map(String::as_str).collect()
    }

    fn delete(mut self) -> Result<(), TyrionError> {
        self.remove()
    }

    fn remove(&mut self) -> Result<(), TyrionError> {
        let deadline = unix_timestamp()?.saturating_add(60);
        for relay in std::mem::take(&mut self.relays) {
            self.runtime.delete_container(&relay)?;
        }
        for network in [self.egress.clone(), self.internal.clone()] {
            let output = self
                .runtime
                .docker(&["network", "rm", &network], deadline)?;
            if !output.status.success() && !reports_missing(&output.stderr) {
                return Err(command_failure(
                    "Docker network removal",
                    output.status,
                    &output.stderr,
                ));
            }
        }
        Ok(())
    }
}

impl ContainedCodexRuntime {
    fn docker(&self, arguments: &[&str], deadline: i64) -> Result<Output, TyrionError> {
        ensure_lease_active(deadline)?;
        run_until(self.docker_command(arguments), deadline)
    }

    fn docker_checked(&self, arguments: &[&str], deadline: i64) -> Result<Output, TyrionError> {
        let output = self.docker(arguments, deadline)?;
        require_success("Docker", output)
    }

    /// Every Docker invocation runs with a cleared environment, an explicit
    /// daemon address rather than an ambient context, and a Tyrion-owned CLI
    /// configuration that carries no registry credential.
    fn docker_command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(&self.config.docker_binary);
        command
            .args(arguments)
            .env_clear()
            .env("PATH", DOCKER_PATH)
            .env("DOCKER_HOST", &self.config.docker_host)
            .env("DOCKER_CONFIG", self.data_dir.join("docker"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Forward the declared credentials by name only. Docker reads each value
    /// from this process, so it never appears in a command line, in the
    /// container's persistent environment, or in Tyrion's durable state.
    fn credential_arguments(&self) -> Vec<String> {
        self.config
            .worker_credentials
            .iter()
            .flat_map(|variable| ["--env".to_owned(), variable.clone()])
            .collect()
    }

    fn apply_credentials(&self, command: &mut Command) {
        for variable in &self.config.worker_credentials {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
    }

    /// Remove a container and confirm its absence independently. `docker exec`
    /// is never used as a liveness probe because it restarts a stopped
    /// container.
    fn delete_container(&self, name: &str) -> Result<(), TyrionError> {
        let deadline = unix_timestamp()?.saturating_add(60);
        let removal = self.docker(&["rm", "--force", "--volumes", name], deadline)?;
        if !removal.status.success() && !reports_missing(&removal.stderr) {
            return Err(command_failure(
                "Docker container removal",
                removal.status,
                &removal.stderr,
            ));
        }
        let present = self.docker(&["inspect", "--type", "container", name], deadline)?;
        if present.status.success() {
            return Err(TyrionError::SecurityInvariantViolation(format!(
                "Docker container {name} survived forced removal"
            )));
        }
        Ok(())
    }
}

/// Removal is idempotent from Tyrion's side: a container that is absent, or
/// that another sweep is already tearing down, is as good as removed. Only an
/// unexplained failure is worth surfacing.
fn reports_missing(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("no such container")
        || stderr.contains("not found")
        || stderr.contains("already in progress")
}

fn text_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn validate_config(config: &RuntimeConfig) -> Result<(), TyrionError> {
    if config.codex_version != CODEX_VERSION {
        return Err(TyrionError::InvalidRequest(
            "Codex Worker configuration does not match the pinned Codex version".into(),
        ));
    }
    if config.vcpus != 2
        || config.memory_mib != 6144
        || config.writable_storage_mib != 4096
        || config.max_processes != 256
    {
        return Err(TyrionError::InvalidRequest(
            "Worker containment must use 2 vCPUs, 6144 MiB combined memory, 4096 MiB writable storage, and 256 processes".into(),
        ));
    }
    if config.writable_storage_mib >= config.memory_mib {
        return Err(TyrionError::InvalidRequest(
            "the writable tmpfs is charged to the memory cgroup and must be smaller than it".into(),
        ));
    }
    if config.lease_ttl_seconds == 0 || config.lease_ttl_seconds > 3600 {
        return Err(TyrionError::InvalidRequest(
            "Worker Lease TTL must be between 1 and 3600 seconds".into(),
        ));
    }
    if config.model.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "Codex model is required".into(),
        ));
    }
    if config.docker_host.trim().is_empty() {
        return Err(TyrionError::InvalidRequest(
            "an explicit Docker daemon address is required; Tyrion never resolves an ambient context".into(),
        ));
    }
    if !is_content_addressed(&config.worker_image) {
        return Err(TyrionError::InvalidRequest(
            "the Worker image must be content addressed: a registry digest reference or a bare sha256 image id".into(),
        ));
    }
    if !is_image_id(&config.worker_image_id) {
        return Err(TyrionError::InvalidRequest(
            "the expected Worker image identity must be a lowercase sha256 digest".into(),
        ));
    }
    if let Some(egress) = &config.egress {
        if egress.destinations.is_empty() {
            return Err(TyrionError::InvalidRequest(
                "brokered egress must name at least one destination or be omitted".into(),
            ));
        }
        for destination in &egress.destinations {
            if destination.host.trim().is_empty()
                || destination.host.contains(char::is_whitespace)
                || destination.port == 0
            {
                return Err(TyrionError::InvalidRequest(
                    "each brokered egress destination needs a host and a nonzero port".into(),
                ));
            }
        }
    }
    for variable in &config.worker_credentials {
        if variable.trim().is_empty()
            || !variable
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(TyrionError::InvalidRequest(
                "each forwarded Worker credential must name one environment variable".into(),
            ));
        }
        if std::env::var_os(variable).is_none() {
            return Err(TyrionError::InvalidRequest(format!(
                "Worker credential {variable} is not present in the daemon environment"
            )));
        }
    }
    verify_hash(&config.docker_binary, &config.docker_sha256)?;
    verify_hash(&config.codex_binary, &config.codex_sha256)?;
    if let Some(host) = &config.codex_code_mode_host {
        verify_hash(&host.path, &host.sha256)?;
    }
    if let Some(claude) = &config.claude {
        if claude.version.trim().is_empty() {
            return Err(TyrionError::InvalidRequest(
                "the Claude runtime profile requires a pinned version".into(),
            ));
        }
        verify_hash(&claude.binary, &claude.sha256)?;
    }
    if let Some(pi) = &config.pi {
        if pi.model_provider != "openai"
            || !pi.model.starts_with("openai/")
            || pi.model.len() == "openai/".len()
            || pi.version.trim().is_empty()
        {
            return Err(TyrionError::InvalidRequest(
                "the Pi runtime profile requires the qualified OpenAI provider and model".into(),
            ));
        }
        verify_hash(&pi.binary, &pi.sha256)?;
    }
    let version = Command::new(&config.docker_binary)
        .arg("--version")
        .env_clear()
        .env("PATH", DOCKER_PATH)
        .env("DOCKER_HOST", &config.docker_host)
        .output()?;
    let version = require_success("Docker version probe", version)?;
    if String::from_utf8_lossy(&version.stdout).trim() != config.docker_version {
        return Err(TyrionError::InvalidRequest(
            "the Docker CLI version does not match its pin".into(),
        ));
    }
    Ok(())
}

/// The Worker image must be present locally and must be exactly the pinned
/// one. Tyrion never pulls, so provisioning stays an explicit operator step.
fn validate_worker_image(config: &RuntimeConfig, data_dir: &Path) -> Result<(), TyrionError> {
    let resolved = Command::new(&config.docker_binary)
        .args([
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            &config.worker_image,
        ])
        .env_clear()
        .env("PATH", DOCKER_PATH)
        .env("DOCKER_HOST", &config.docker_host)
        .env("DOCKER_CONFIG", data_dir.join("docker"))
        .output()?;
    let resolved = require_success("Docker Worker image probe", resolved)?;
    let resolved = String::from_utf8_lossy(&resolved.stdout).trim().to_owned();
    if resolved != config.worker_image_id {
        return Err(TyrionError::InvalidRequest(format!(
            "the provisioned Worker image is {resolved}, not the pinned {}",
            config.worker_image_id
        )));
    }
    Ok(())
}

/// A reference Tyrion will accept: a registry digest reference, or the bare
/// image id of a locally built image. Both name exact content; a tag does not.
/// A personal runtime builds its own Worker image and never pushes it, so
/// requiring a registry would rule that out without improving the pin.
fn is_content_addressed(reference: &str) -> bool {
    if is_image_id(reference) {
        return true;
    }
    match reference.split_once("@sha256:") {
        Some((repository, digest)) => !repository.is_empty() && is_hex64(digest),
        None => false,
    }
}

fn is_image_id(identity: &str) -> bool {
    identity.strip_prefix("sha256:").is_some_and(is_hex64)
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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
    validate_no_escaping_symlink(&quarantine, &candidate_revision)?;
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

/// The direct Codex slice. Credentials reach it only as the named variables
/// the Principal declared, delivered by Docker for this one execution.
fn attempt_script(
    base_revision: &str,
    model: &str,
    allow_empty_changes: bool,
    credentials: &[String],
) -> String {
    let names = credentials.join(" ");
    let auth_setup = if credentials.is_empty() {
        "codex_credential_env=".to_owned()
    } else {
        let forwarded = credentials
            .iter()
            .map(|name| format!("{name}=\"${name}\""))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            r#"for name in {names}; do
  eval "value=\${{$name:-}}"
  if [ -z "$value" ]; then
    echo "missing brokered Worker credential $name" >&2
    exit 42
  fi
done
codex_credential_env="{forwarded}""#
        )
    };
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
env -i PATH=/usr/local/bin:/usr/bin:/bin HOME="$root/home" CODEX_HOME="$root/home/.codex" \
  TMPDIR="$root/tmp" $codex_credential_env \
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

/// A Worker may be authorized to write a path while having no authority to
/// read what a symlink at that path would point at. Integration must never
/// materialize a link that leaves the repository, because a Principal who
/// later merges the artifact would resolve it against their own machine.
fn validate_no_escaping_symlink(
    repository: &Path,
    candidate_revision: &str,
) -> Result<(), TyrionError> {
    let listing = git_bytes(
        repository,
        &[os("ls-tree"), os("-r"), os("-z"), os(candidate_revision)],
    )?;
    for entry in listing.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let entry = String::from_utf8(entry.to_vec()).map_err(|_| {
            TyrionError::InvalidRequest("Result contains a non-UTF-8 tree entry".into())
        })?;
        // "<mode> <type> <object>\t<path>"
        let Some((metadata, path)) = entry.split_once('\t') else {
            return Err(TyrionError::InvalidRequest(
                "Result contains an unparseable tree entry".into(),
            ));
        };
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(TyrionError::InvalidRequest(
                "Result contains an unparseable tree entry".into(),
            ));
        }
        if fields[0] != "120000" {
            continue;
        }
        let target = git_bytes(repository, &[os("cat-file"), os("blob"), os(fields[2])])?;
        let target = String::from_utf8(target).map_err(|_| {
            TyrionError::InvalidRequest("Result contains a non-UTF-8 symlink target".into())
        })?;
        if symlink_escapes_repository(path, &target) {
            return Err(TyrionError::InvalidRequest(format!(
                "Codex Result symlink {path} points outside the repository at {target}"
            )));
        }
    }
    Ok(())
}

/// Resolve a symlink target against the directory holding it, purely
/// lexically, and report whether it leaves the repository root.
fn symlink_escapes_repository(path: &str, target: &str) -> bool {
    let target = target.trim_end_matches('\n');
    if target.is_empty() || Path::new(target).is_absolute() {
        return true;
    }
    let mut resolved: Vec<&str> = Path::new(path)
        .parent()
        .map(|parent| {
            parent
                .to_str()
                .unwrap_or_default()
                .split('/')
                .filter(|segment| !segment.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if resolved.pop().is_none() {
                    return true;
                }
            }
            segment => resolved.push(segment),
        }
    }
    false
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
    // Transfers redirect one stream straight to a file, so neither reader is
    // guaranteed to exist.
    // Short Docker commands dominate the seam, so poll quickly and back off.
    let mut poll_interval = Duration::from_micros(200);
    let stdout_reader = child
        .stdout
        .take()
        .map(|stdout| thread::spawn(move || read_all(stdout)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|stderr| thread::spawn(move || read_all(stderr)));
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
        thread::sleep(poll_interval);
        poll_interval = (poll_interval * 2).min(Duration::from_millis(20));
    }
}

fn read_all(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(
    reader: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, TyrionError> {
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
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
    use super::{
        attempt_script, is_content_addressed, is_image_id, result_schema,
        symlink_escapes_repository,
    };

    #[test]
    fn a_worker_without_declared_credentials_receives_none() {
        let script = attempt_script(
            "0123456789012345678901234567890123456789",
            "test-model",
            false,
            &[],
        );
        assert!(script.contains("codex_credential_env="));
        for absent in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "GH_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            assert!(!script.contains(absent), "unexpectedly forwarded {absent}");
        }
    }

    #[test]
    fn declared_credentials_are_required_and_forwarded_by_name() {
        let script = attempt_script(
            "0123456789012345678901234567890123456789",
            "test-model",
            false,
            &["OPENAI_API_KEY".to_owned()],
        );
        assert!(script.contains("for name in OPENAI_API_KEY; do"));
        assert!(script.contains("missing brokered Worker credential"));
        assert!(script.contains("codex_credential_env=\"OPENAI_API_KEY=\"$OPENAI_API_KEY\"\""));
        assert!(!script.contains("openshell:resolve:env:"));
    }

    #[test]
    fn only_a_content_addressed_worker_image_is_accepted() {
        let digest = "a".repeat(64);
        assert!(is_content_addressed(&format!(
            "registry.example/worker@sha256:{digest}"
        )));
        // A locally built image has no registry digest, only an id.
        assert!(is_content_addressed(&format!("sha256:{digest}")));
        assert!(!is_content_addressed("registry.example/worker:latest"));
        assert!(!is_content_addressed(
            "registry.example/worker@sha256:short"
        ));
        assert!(!is_content_addressed(&format!("@sha256:{digest}")));
        assert!(is_image_id(&format!("sha256:{digest}")));
        assert!(!is_image_id(&digest));
        assert!(!is_image_id(&format!("sha256:{}", "A".repeat(64))));
    }

    #[test]
    fn only_symlinks_that_stay_inside_the_repository_are_accepted() {
        // Inside the repository: legitimate and allowed.
        assert!(!symlink_escapes_repository("src/link", "module.rs"));
        assert!(!symlink_escapes_repository("src/a/link", "../b/module.rs"));
        assert!(!symlink_escapes_repository("src/link", "./module.rs"));
        assert!(!symlink_escapes_repository("a/b/c/link", "../../../top.rs"));

        // Leaving the repository, by any route.
        assert!(symlink_escapes_repository(
            "issue-4.txt",
            "/Users/someone/.ssh/id_rsa"
        ));
        assert!(symlink_escapes_repository("issue-4.txt", ".."));
        assert!(symlink_escapes_repository("src/link", "../../etc/passwd"));
        assert!(symlink_escapes_repository(
            "a/b/c/link",
            "../../../../etc/passwd"
        ));
        assert!(symlink_escapes_repository("link", "../anything"));
        assert!(symlink_escapes_repository("link", ""));
        // A trailing newline must not disguise an absolute target.
        assert!(symlink_escapes_repository("link", "/etc/passwd\n"));
    }

    #[test]
    fn contained_codex_schema_forbids_external_effects() {
        let schema: serde_json::Value =
            serde_json::from_slice(result_schema()).expect("result schema is valid JSON");
        assert_eq!(schema["properties"]["known_effects"]["maxItems"], 0);
    }
}

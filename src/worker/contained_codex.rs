use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    ArtifactRecord, AssignmentContext, CriterionDefinition, VerificationKind, VerificationRecord,
    VerificationScope,
};
use crate::domain::EvidenceOutcome;
use crate::protocol::Verifier;
use crate::TyrionError;

const SOURCE_REVISION: &str = "dd2b4e3bc0688bdd59f90030f7c1d52511d6e354";
const BASE_IMAGE: &str = "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e";
const POLICY_SHA256: &str = "a245a499c89f4edf39203935d83ae5fd8e3209e121da78a2eaaa0ff82512a469";
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
    base_image: String,
    codex_binary: PathBuf,
    codex_version: String,
    codex_sha256: String,
    model: String,
    openshell_provider: String,
    lease_ttl_seconds: u64,
    vcpus: u32,
    memory_mib: u64,
    overlay_disk_mib: u64,
    max_processes: u32,
}

#[derive(Debug, Deserialize)]
struct PinnedArtifact {
    path: PathBuf,
    sha256: String,
}

pub(super) struct ContainedCodexRuntime {
    config: RuntimeConfig,
    data_dir: PathBuf,
    configuration: String,
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
}

pub(super) struct GitIntegrated {
    pub integrated_revision: String,
    pub artifacts: Vec<ArtifactRecord>,
    pub state: GitIntegratedState,
}

pub(super) struct GitIntegratedState {
    integrated_bundle: PathBuf,
}

impl ContainedCodexRuntime {
    pub(super) fn load(config_path: &Path, data_dir: &Path) -> Result<Self, TyrionError> {
        let encoded = fs::read(config_path)?;
        let config: RuntimeConfig = serde_json::from_slice(&encoded)?;
        validate_config(&config)?;
        let fingerprint = format!("{:x}", Sha256::digest(&encoded));
        let configuration = format!(
            "{} + {} + openshell-source:{} + microvm-profile:{}",
            config.codex_version,
            config.openshell_version,
            config.source_revision,
            &fingerprint[..16]
        );
        Ok(Self {
            config,
            data_dir: data_dir.to_owned(),
            configuration,
        })
    }

    pub(super) fn configuration(&self) -> String {
        self.configuration.clone()
    }

    pub(super) fn lease_ttl_seconds(&self) -> u64 {
        self.config.lease_ttl_seconds
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
        enforce_storage_ceiling(&[&base_bundle], assignment.max_storage_bytes)?;

        let sandbox_name = sandbox_name("attempt", &assignment.attempt_id);
        let sandbox = Sandbox::create(self, &sandbox_name, assignment.lease_expires_at)?;
        sandbox.preflight(&repository, &self.data_dir, assignment.lease_expires_at)?;
        sandbox.upload(
            &base_bundle,
            "/sandbox/base.bundle",
            assignment.lease_expires_at,
        )?;
        sandbox.upload(
            &self.config.codex_binary,
            "/sandbox/codex",
            assignment.lease_expires_at,
        )?;
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
            attempt_script(base_revision, &self.config.model),
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
        enforce_storage_ceiling(
            &[&base_bundle, &candidate_bundle],
            assignment.max_storage_bytes,
        )?;
        let validated = validate_candidate_bundle(
            &artifact_dir,
            &base_bundle,
            &candidate_bundle,
            base_revision,
            &assignment.authorized_paths,
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
            candidate_commits: validated.commits,
            changed_paths: validated.changed_paths,
            artifacts: vec![base_artifact, candidate_artifact],
            known_effects,
            state: GitCandidateState {
                base_bundle,
                candidate_bundle,
                base_revision: base_revision.to_owned(),
                candidate_revision: validated.candidate_revision,
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
        if current.trim() != candidate.base_revision {
            return Err(TyrionError::InvalidRequest(format!(
                "integration base is {}, but Result requires {}",
                current.trim(),
                candidate.base_revision
            )));
        }
        git_checked(
            Some(&repository),
            &[
                os("fetch"),
                os("--quiet"),
                candidate.candidate_bundle.as_os_str().to_owned(),
                os("refs/heads/tyrion-result:refs/heads/tyrion-candidate"),
            ],
        )?;
        git_checked(
            Some(&repository),
            &[os("merge"), os("--ff-only"), os("tyrion-candidate")],
        )?;
        let integrated_revision = git_text(&repository, &[os("rev-parse"), os("HEAD")])?
            .trim()
            .to_owned();
        if integrated_revision != candidate.candidate_revision {
            return Err(TyrionError::InvalidRequest(
                "integrated revision does not match the accepted candidate".into(),
            ));
        }
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
            state: GitIntegratedState { integrated_bundle },
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

    fn verify_bundles(
        &self,
        assignment: &AssignmentContext,
        scope: VerificationScope,
        base: Option<(&Path, &str)>,
        result_bundle: &Path,
        revision: &str,
    ) -> Result<Vec<VerificationRecord>, TyrionError> {
        ensure_lease_active(assignment.lease_expires_at)?;
        let sandbox_name = sandbox_name(scope.as_str(), &assignment.attempt_id);
        let sandbox = Sandbox::create(self, &sandbox_name, assignment.lease_expires_at)?;
        let crate::protocol::ExecutionSpec::CodexGit { repository, .. } = &assignment.execution
        else {
            unreachable!("Git verification has a codex_git execution spec")
        };
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

        let mut records = Vec::with_capacity(assignment.criteria.len());
        for criterion in &assignment.criteria {
            records.push(sandbox.verify_command(criterion, scope, assignment.lease_expires_at)?);
        }
        sandbox.delete()?;
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
            path_text(&runtime.config.policy_path)?,
        ];
        arguments.extend(["--provider", &runtime.config.openshell_provider]);
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
            "set -eu; printf tyrion-containment-probe; test \"$(cat /sys/fs/cgroup/pids.max)\" = 256; test \"$(getconf _NPROCESSORS_ONLN)\" = 2; memory_kib=$(awk '/MemTotal/ {{print $2}}' /proc/meminfo); test \"$memory_kib\" -ge 1900000; test \"$memory_kib\" -le 2097152; storage_kib=$(df -Pk /sandbox | awk 'NR==2 {{print $2}}'); test \"$storage_kib\" -le 4194304; test ! -e {host_repository}; test ! -e {host_repository_parent}; test ! -e {host_state}; test ! -e /var/run/docker.sock; test ! -e /run/containerd/containerd.sock; test ! -e /home/sandbox/.ssh; test ! -e /home/sandbox/.aws; test ! -e /home/sandbox/.config/gh; test ! -e /home/sandbox/.codex; test -z \"${{OPENAI_API_KEY:-}}${{AWS_ACCESS_KEY_ID:-}}${{GH_TOKEN:-}}${{GITHUB_TOKEN:-}}${{SSH_AUTH_SOCK:-}}\"; test ! -r /opt/openshell/auth/sandbox.jwt; test ! -r /opt/openshell/tls/tls.key; if printf denied >/etc/tyrion-probe 2>/dev/null; then exit 91; fi; printf allowed >/sandbox/tyrion-probe; command -v curl >/dev/null; if curl -fsS --max-time 5 https://example.com >/dev/null 2>&1; then exit 92; fi; sleep 600 >/dev/null 2>&1 & descendant=$!; kill -0 \"$descendant\"; printf descendant-live"
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
            verifier_kind: VerificationKind::Command,
            scope,
            outcome: if output.status.success() {
                EvidenceOutcome::Passed
            } else {
                EvidenceOutcome::Failed
            },
            observed,
            expected: serde_json::to_string(argv)?,
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
    verify_hash(&config.codex_binary, &config.codex_sha256)?;
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
    for required in [
        "CONFIG_SECURITY=y",
        "CONFIG_SECURITY_LANDLOCK=y",
        "CONFIG_CGROUP_PIDS=y",
        "CONFIG_SECCOMP_FILTER=y",
    ] {
        if !kernel.lines().any(|line| line == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "kernel configuration is missing {required}"
            )));
        }
    }
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
    let codex = Command::new(&config.codex_binary)
        .arg("--version")
        .env_clear()
        .output()?;
    let codex = require_success("Codex version probe", codex)?;
    if String::from_utf8_lossy(&codex.stdout).trim() != config.codex_version {
        return Err(TyrionError::InvalidRequest(
            "Codex binary version does not match its pin".into(),
        ));
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
    if changed_paths.is_empty() {
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

fn attempt_script(base_revision: &str, model: &str) -> String {
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
cat >"$root/home/.codex/auth.json" <<'AUTH'
{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {
    "id_token": "openshell:resolve:env:CODEX_AUTH_ID_TOKEN",
    "access_token": "openshell:resolve:env:CODEX_AUTH_ACCESS_TOKEN",
    "refresh_token": "openshell:resolve:env:CODEX_AUTH_REFRESH_TOKEN",
    "account_id": "openshell:resolve:env:CODEX_AUTH_ACCOUNT_ID"
  }
}
AUTH
chmod 600 "$root/home/.codex/auth.json"
codex_auth_env="CODEX_AUTH_ACCESS_TOKEN=$CODEX_AUTH_ACCESS_TOKEN CODEX_AUTH_REFRESH_TOKEN=$CODEX_AUTH_REFRESH_TOKEN CODEX_AUTH_ACCOUNT_ID=$CODEX_AUTH_ACCOUNT_ID CODEX_AUTH_ID_TOKEN=$CODEX_AUTH_ID_TOKEN"
"#;
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
  "$root/codex" exec --json --ephemeral --ignore-user-config \
  --dangerously-bypass-approvals-and-sandbox -C "$root/repository" \
  --model {model} --output-schema "$root/result-schema.json" \
  --output-last-message "$root/codex-result.json" - \
  <"$root/prompt.txt" >"$root/codex-events.jsonl"
git -C "$root/repository" add -A
if git -C "$root/repository" diff --cached --quiet; then
  echo 'Codex produced no changes' >&2
  exit 41
fi
env -i PATH=/usr/local/bin:/usr/bin:/bin \
  GIT_AUTHOR_NAME=Tyrion GIT_AUTHOR_EMAIL=worker@tyrion.invalid \
  GIT_COMMITTER_NAME=Tyrion GIT_COMMITTER_EMAIL=worker@tyrion.invalid \
  git -C "$root/repository" commit -qm 'feat: implement assignment'
git -C "$root/repository" branch -f tyrion-result HEAD
git -C "$root/repository" bundle create "$root/candidate.bundle" \
  refs/heads/tyrion-result ^{base}
sync
"#,
        base = shell_quote(base_revision),
        model = shell_quote(model),
        auth_setup = auth_setup,
    )
}

fn worker_prompt(assignment: &AssignmentContext, base_revision: &str) -> String {
    format!(
        "Implement this Assignment in the current Git repository.\n\nGoal: {}\n\nMandate revision: {}\nImmutable base: {}\nAllowed changed paths: {}\n\nDo not use credentials or perform external effects. Return only the required structured final response.",
        assignment.goal,
        assignment.mandate_revision,
        base_revision,
        assignment.authorized_paths.join(", ")
    )
}

fn result_schema() -> &'static [u8] {
    br#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "summary": {"type": "string"},
    "known_effects": {"type": "array", "items": {"type": "string"}}
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
    let short = attempt_id.chars().take(12).collect::<String>();
    format!(
        "tyrion-{scope}-{short}-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    )
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

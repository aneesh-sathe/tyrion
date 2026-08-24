use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::protocol::{CredentialUseMode, OperationRequest};
use crate::TyrionError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialRuntimeConfig {
    keychain: KeychainConfig,
    broker: BrokerConfig,
    #[serde(default)]
    effect_sandbox: Option<EffectSandboxConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeychainConfig {
    security_binary: PathBuf,
    security_sha256: String,
    keychain_path: PathBuf,
    service: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerConfig {
    curl_binary: PathBuf,
    curl_sha256: String,
    destinations: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectSandboxConfig {
    openshell_binary: PathBuf,
    openshell_sha256: String,
    openshell_version: String,
    openshell_config_home: PathBuf,
    base_image: String,
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
    adapter_binary: PathBuf,
    adapter_sha256: String,
    adapter_version: String,
    destination: String,
    vcpus: u32,
    memory_mib: u64,
    overlay_disk_mib: u64,
    max_processes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedArtifact {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialEffectBinding {
    pub(crate) kind: String,
    pub(crate) runtime_sha256: String,
    pub(crate) destination: String,
    pub(crate) resolved_url: String,
}

pub(crate) struct CredentialRuntime {
    config: CredentialRuntimeConfig,
    fingerprint: String,
}

pub(crate) enum CredentialEffectError {
    Failed(TyrionError),
    Uncertain { error: TyrionError, receipt: Value },
    LeaveStartedAfterEffect,
}

#[derive(Clone, Copy)]
pub(crate) struct CredentialExecutionDeadline {
    started: Instant,
    max_duration: Duration,
    authority_expires_at: i64,
    credential_expires_at: i64,
}

impl CredentialExecutionDeadline {
    pub(crate) fn new(
        started: Instant,
        max_duration_seconds: u64,
        authority_expires_at: i64,
        credential_expires_at: i64,
    ) -> Self {
        Self {
            started,
            max_duration: Duration::from_secs(max_duration_seconds),
            authority_expires_at,
            credential_expires_at,
        }
    }

    fn remaining(self) -> Result<Duration, TyrionError> {
        let duration_remaining = self.max_duration.saturating_sub(self.started.elapsed());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TyrionError::InvalidRequest("system clock precedes Unix epoch".into()))?;
        let wall_deadline = self.authority_expires_at.min(self.credential_expires_at);
        let wall_remaining = u64::try_from(wall_deadline)
            .ok()
            .map(Duration::from_secs)
            .and_then(|deadline| deadline.checked_sub(now));
        let Some(wall_remaining) = wall_remaining else {
            return Err(TyrionError::ControlDenied(
                "credentialed effect reached its duration, Worker Lease, Commission, or credential deadline"
                    .into(),
            ));
        };
        if duration_remaining.is_zero() || wall_remaining.is_zero() {
            return Err(TyrionError::ControlDenied(
                "credentialed effect reached its duration, Worker Lease, Commission, or credential deadline"
                    .into(),
            ));
        }
        Ok(duration_remaining.min(wall_remaining))
    }

    fn instant_deadline(self) -> Result<Instant, TyrionError> {
        Instant::now()
            .checked_add(self.remaining()?)
            .ok_or_else(|| {
                TyrionError::InvalidRequest("credentialed effect deadline exceeds Instant".into())
            })
    }
}

impl From<TyrionError> for CredentialEffectError {
    fn from(error: TyrionError) -> Self {
        Self::Failed(error)
    }
}

impl CredentialRuntime {
    pub(crate) fn load(path: &Path) -> Result<Self, TyrionError> {
        let encoded = fs::read(path)?;
        let config: CredentialRuntimeConfig = serde_json::from_slice(&encoded)?;
        validate_hash(
            &config.keychain.security_binary,
            &config.keychain.security_sha256,
        )?;
        validate_hash(&config.broker.curl_binary, &config.broker.curl_sha256)?;
        if !config.keychain.keychain_path.is_file()
            || config.keychain.service.trim().is_empty()
            || config.keychain.service.contains('\0')
            || config.broker.destinations.is_empty()
        {
            return Err(TyrionError::InvalidRequest(
                "credential runtime requires an existing Keychain, service, and destination".into(),
            ));
        }
        for (name, destination) in &config.broker.destinations {
            validate_destination(name, destination)?;
        }
        if let Some(sandbox) = &config.effect_sandbox {
            validate_effect_sandbox(sandbox, &config.broker)?;
        }
        Ok(Self {
            config,
            fingerprint: format!("{:x}", Sha256::digest(&encoded)),
        })
    }

    pub(crate) fn supports_grant(
        &self,
        credential_reference: &str,
        capability: &str,
        destination: &str,
    ) -> Result<(), TyrionError> {
        validate_reference(credential_reference)?;
        if capability != "http_bearer" {
            return Err(TyrionError::ControlDenied(
                "the credential runtime does not provide the requested capability".into(),
            ));
        }
        if !self.config.broker.destinations.contains_key(destination) {
            return Err(TyrionError::ControlDenied(
                "the credential runtime does not provide the exact destination".into(),
            ));
        }
        let status = self
            .keychain_command("find-generic-password", credential_reference, false)?
            .status;
        if !status.success() {
            return Err(TyrionError::ControlDenied(
                "the operating-system credential store does not contain the requested reference"
                    .into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn supports_exposure(&self, destination: &str) -> Result<(), TyrionError> {
        let sandbox = self.config.effect_sandbox.as_ref().ok_or_else(|| {
            TyrionError::ControlDenied("one-shot credential exposure is not configured".into())
        })?;
        validate_effect_sandbox(sandbox, &self.config.broker)?;
        if sandbox.destination != destination {
            return Err(TyrionError::ControlDenied(
                "the Effect Sandbox does not permit the exact Credential Grant destination".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn bind(
        &self,
        operation: &OperationRequest,
    ) -> Result<CredentialEffectBinding, TyrionError> {
        let destination = operation.destination.as_deref().ok_or_else(|| {
            TyrionError::InvalidRequest("credentialed effects require a destination".into())
        })?;
        let base = self
            .config
            .broker
            .destinations
            .get(destination)
            .ok_or_else(|| {
                TyrionError::ControlDenied(
                    "the credentialed effect destination is not configured".into(),
                )
            })?;
        Ok(CredentialEffectBinding {
            kind: match operation.operation.as_str() {
                "credential.http.request" => "credentialed_http_v1",
                "credential.command.request" => "credentialed_command_v1",
                _ => {
                    return Err(TyrionError::ControlDenied(
                        "the credential runtime does not support this operation".into(),
                    ))
                }
            }
            .into(),
            runtime_sha256: self.fingerprint.clone(),
            destination: destination.to_owned(),
            resolved_url: format!("{base}/{}", operation.target),
        })
    }

    pub(crate) fn validate_operation(
        &self,
        operation: &OperationRequest,
    ) -> Result<(), TyrionError> {
        let credential_use = operation.credential.as_ref().ok_or_else(|| {
            TyrionError::InvalidRequest("credentialed effect requires a Credential Grant".into())
        })?;
        let expected_mode = match operation.operation.as_str() {
            "credential.http.request" => CredentialUseMode::Brokered,
            "credential.command.request" => CredentialUseMode::OneShotExposure,
            _ => {
                return Err(TyrionError::ControlDenied(
                    "the credential runtime does not support this operation".into(),
                ))
            }
        };
        if credential_use.mode != expected_mode
            || !valid_request_parameters(&operation.parameters)
            || operation.parameters.get("method").map(String::as_str) != Some("POST")
            || operation
                .parameters
                .get("reconciliation_target")
                .map(String::as_str)
                != Some(operation.target.as_str())
            || operation
                .parameters
                .get("content_type")
                .is_none_or(|value| value.contains(['\r', '\n', '\0']))
        {
            return Err(TyrionError::InvalidRequest(
                "credentialed effect parameters or delivery mode are not exact and supported"
                    .into(),
            ));
        }
        for field in [
            "confirmed_reconciliation_sha256",
            "not_applied_reconciliation_sha256",
        ] {
            if let Some(digest) = operation.parameters.get(field) {
                if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(TyrionError::InvalidRequest(format!(
                        "{field} must be a SHA-256 digest"
                    )));
                }
            }
        }
        self.bind(operation)?;
        Ok(())
    }

    pub(crate) fn ensure_binding(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
    ) -> Result<(), TyrionError> {
        let expected_kind = match operation.operation.as_str() {
            "credential.http.request" => "credentialed_http_v1",
            "credential.command.request" => "credentialed_command_v1",
            _ => "unsupported",
        };
        if binding.kind != expected_kind
            || binding.runtime_sha256 != self.fingerprint
            || self.bind(operation)? != *binding
        {
            return Err(TyrionError::ControlDenied(
                "the credentialed destination binding changed after approval".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn execute_brokered(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
        credential_reference: &str,
        operation_request_id: &str,
        deadline: CredentialExecutionDeadline,
        process_started: &mut dyn FnMut(u32, &str) -> Result<(), TyrionError>,
    ) -> Result<Value, CredentialEffectError> {
        let execution = (|| {
            if operation.operation != "credential.http.request"
                || operation.effect.as_deref() != Some("external.write")
                || operation
                    .credential
                    .as_ref()
                    .map(|credential_use| credential_use.mode)
                    != Some(CredentialUseMode::Brokered)
            {
                return Err(CredentialEffectError::Failed(TyrionError::ControlDenied(
                    "only the exact brokered credential.http.request effect can execute".into(),
                )));
            }
            self.revalidate_broker()
                .map_err(CredentialEffectError::Failed)?;
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?;
            self.ensure_binding(operation, binding)
                .map_err(CredentialEffectError::Failed)?;
            let secret = self
                .read_secret(credential_reference)
                .map_err(CredentialEffectError::Failed)?;
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?;
            self.run_curl(
                operation,
                binding,
                &secret,
                operation_request_id,
                deadline,
                process_started,
            )
        })();
        if matches!(
            execution,
            Err(CredentialEffectError::LeaveStartedAfterEffect)
        ) {
            return execution;
        }
        let revocation = self.revoke_and_verify(credential_reference);
        match (execution, revocation) {
            (Ok(mut receipt), Ok(())) => {
                receipt["credential_revocation"] = Value::String("verified_absent".into());
                Ok(receipt)
            }
            (Err(CredentialEffectError::Uncertain { error, mut receipt }), Ok(())) => {
                receipt["credential_revocation"] = Value::String("verified_absent".into());
                Err(CredentialEffectError::Uncertain { error, receipt })
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(receipt), Err(error)) => Err(CredentialEffectError::Uncertain {
                error,
                receipt: serde_json::json!({
                    "status": "uncertain",
                    "external_response": receipt,
                    "credential_revocation": "unverified",
                    "secret_material_retained": false,
                    "requirement": "Revoke the exact credential and reconcile the external destination read-only before resuming."
                }),
            }),
            (Err(CredentialEffectError::Failed(error)), Err(_)) => {
                Err(CredentialEffectError::Uncertain {
                    error,
                    receipt: serde_json::json!({
                        "status": "uncertain",
                        "credential_revocation": "unverified",
                        "secret_material_retained": false,
                        "requirement": "Revoke the exact credential and reconcile the external destination read-only before resuming."
                    }),
                })
            }
            (Err(CredentialEffectError::Uncertain { error, receipt }), Err(_)) => {
                Err(CredentialEffectError::Uncertain { error, receipt })
            }
            (Err(CredentialEffectError::LeaveStartedAfterEffect), _) => {
                Err(CredentialEffectError::LeaveStartedAfterEffect)
            }
        }
    }

    pub(crate) fn execute_one_shot(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
        credential_reference: &str,
        operation_request_id: &str,
        deadline: CredentialExecutionDeadline,
        leave_started_before_cleanup: bool,
    ) -> Result<Value, CredentialEffectError> {
        let execution = (|| {
            if operation.operation != "credential.command.request"
                || operation.effect.as_deref() != Some("external.write")
                || operation
                    .credential
                    .as_ref()
                    .map(|credential_use| credential_use.mode)
                    != Some(CredentialUseMode::OneShotExposure)
            {
                return Err(CredentialEffectError::Failed(TyrionError::ControlDenied(
                    "only the exact one-shot credential.command.request effect can execute".into(),
                )));
            }
            self.revalidate_broker()
                .map_err(CredentialEffectError::Failed)?;
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?;
            self.ensure_binding(operation, binding)
                .map_err(CredentialEffectError::Failed)?;
            self.supports_exposure(&binding.destination)
                .map_err(CredentialEffectError::Failed)?;
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?;
            let secret = self
                .read_secret(credential_reference)
                .map_err(CredentialEffectError::Failed)?;
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?;
            self.run_effect_sandbox(
                operation,
                binding,
                &secret,
                operation_request_id,
                deadline,
                leave_started_before_cleanup,
            )
        })();
        if matches!(
            execution,
            Err(CredentialEffectError::LeaveStartedAfterEffect)
        ) {
            return execution;
        }
        let revocation = self.revoke_and_verify(credential_reference);
        match (execution, revocation) {
            (Ok(mut receipt), Ok(())) => {
                receipt["credential_revocation"] = Value::String("verified_absent".into());
                Ok(receipt)
            }
            (Err(CredentialEffectError::Uncertain { error, mut receipt }), Ok(())) => {
                receipt["credential_revocation"] = Value::String("verified_absent".into());
                Err(CredentialEffectError::Uncertain { error, receipt })
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(receipt), Err(error)) => Err(CredentialEffectError::Uncertain {
                error,
                receipt: serde_json::json!({
                    "status": "uncertain",
                    "external_response": receipt,
                    "credential_revocation": "unverified",
                    "secret_material_retained": false,
                    "requirement": "Revoke the exact exposed credential and reconcile the external destination read-only before resuming."
                }),
            }),
            (Err(CredentialEffectError::Failed(error)), Err(_)) => {
                Err(CredentialEffectError::Uncertain {
                    error,
                    receipt: serde_json::json!({
                        "status": "uncertain",
                        "credential_revocation": "unverified",
                        "secret_material_retained": false,
                        "requirement": "Revoke the exact exposed credential and reconcile the external destination read-only before resuming."
                    }),
                })
            }
            (Err(CredentialEffectError::Uncertain { error, receipt }), Err(_)) => {
                Err(CredentialEffectError::Uncertain { error, receipt })
            }
            (Err(CredentialEffectError::LeaveStartedAfterEffect), _) => {
                Err(CredentialEffectError::LeaveStartedAfterEffect)
            }
        }
    }

    pub(crate) fn observe_read_only(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
    ) -> Result<String, TyrionError> {
        self.revalidate_broker()?;
        self.ensure_binding(operation, binding)?;
        if operation
            .parameters
            .get("reconciliation_target")
            .map(String::as_str)
            != Some(operation.target.as_str())
        {
            return Err(TyrionError::ControlDenied(
                "credentialed reconciliation is not bound to the exact effect target".into(),
            ));
        }
        let max_time = operation.limits.max_duration_seconds.to_string();
        let max_output = operation.limits.max_output_bytes.to_string();
        let mut command = Command::new(&self.config.broker.curl_binary);
        command
            .args([
                "--disable",
                "--silent",
                "--show-error",
                "--max-time",
                &max_time,
                "--max-filesize",
                &max_output,
                "--max-redirs",
                "0",
                "--proto",
                "=http,https",
                "--request",
                "GET",
                &binding.resolved_url,
            ])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_bounded_command(
            &mut command,
            None,
            Duration::from_secs(operation.limits.max_duration_seconds),
            operation.limits.max_output_bytes,
        )?;
        if !output.status.success()
            || output.stdout.len() as u64 > operation.limits.max_output_bytes
        {
            return Err(TyrionError::InvalidRequest(
                "authorized read-only credentialed reconciliation failed".into(),
            ));
        }
        Ok(format!("{:x}", Sha256::digest(&output.stdout)))
    }

    pub(crate) fn revoke_consumed_credential(
        &self,
        credential_reference: &str,
    ) -> Result<(), TyrionError> {
        self.revoke_and_verify(credential_reference)
    }

    pub(crate) fn recover_stranded_effect(
        &self,
        operation_request_id: &str,
        operation: &OperationRequest,
        credential_reference: &str,
        broker_process: Option<(u32, &str)>,
    ) -> Result<Value, TyrionError> {
        let mut receipt = serde_json::json!({
            "credential_revocation": "pending",
            "secret_material_retained": false,
        });
        let containment = if operation
            .credential
            .as_ref()
            .is_some_and(|credential| credential.mode == CredentialUseMode::OneShotExposure)
        {
            (|| {
                let sandbox = self.config.effect_sandbox.as_ref().ok_or_else(|| {
                    TyrionError::ControlDenied(
                        "stranded one-shot cleanup requires its pinned Effect Sandbox runtime"
                            .into(),
                    )
                })?;
                validate_effect_sandbox(sandbox, &self.config.broker)?;
                let sandbox_name = effect_sandbox_name(operation_request_id)?;
                let secret = self.read_secret(credential_reference).ok();
                let mut logs = self.openshell_bounded(
                    sandbox,
                    &["logs", &sandbox_name, "-n", "300"],
                    None,
                    Duration::from_secs(15),
                    1024 * 1024,
                );
                let leak_detected = match (&logs, &secret) {
                    (Ok(logs), Some(secret)) => {
                        contains_bytes(&logs.stdout, secret) || contains_bytes(&logs.stderr, secret)
                    }
                    _ => false,
                };
                let recovery_log_scan = logs.is_ok() && secret.is_some();
                if let Ok(logs) = &mut logs {
                    logs.stdout.zeroize();
                    logs.stderr.zeroize();
                }
                self.delete_sandbox(sandbox, &sandbox_name)?;
                receipt["sandbox_destroyed"] = Value::Bool(true);
                receipt["descendants_terminated"] = Value::Bool(true);
                receipt["secret_leak_detected"] = Value::Bool(leak_detected);
                receipt["recovery_log_scan"] = Value::String(
                    if recovery_log_scan {
                        "completed"
                    } else {
                        "sandbox_or_credential_already_absent"
                    }
                    .into(),
                );
                Ok(())
            })()
        } else {
            let marker = broker_process_marker(operation_request_id)?;
            if broker_process.is_some_and(|process| process.1 != marker) {
                Err(TyrionError::ControlDenied(
                    "durable credential broker process marker does not match the operation".into(),
                ))
            } else {
                self.contain_broker_process(broker_process.map(|process| process.0), &marker)
                    .map(|()| {
                        receipt["broker_process_contained"] = Value::Bool(true);
                        receipt["descendants_terminated"] = Value::Bool(true);
                    })
            }
        };
        let revocation = self.revoke_and_verify(credential_reference);
        if revocation.is_ok() {
            receipt["credential_revocation"] = Value::String("verified_absent".into());
        }
        match (containment, revocation) {
            (Ok(()), Ok(())) => Ok(receipt),
            (Err(containment), Ok(())) => Err(containment),
            (Ok(()), Err(revocation)) => Err(revocation),
            (Err(containment), Err(revocation)) => Err(TyrionError::ControlDenied(format!(
                "credential effect containment failed ({containment}); credential revocation also failed ({revocation})"
            ))),
        }
    }

    fn run_effect_sandbox(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
        secret: &[u8],
        operation_request_id: &str,
        deadline: CredentialExecutionDeadline,
        leave_started_before_cleanup: bool,
    ) -> Result<Value, CredentialEffectError> {
        let sandbox = self.config.effect_sandbox.as_ref().ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::ControlDenied(
                "one-shot credential exposure is not configured".into(),
            ))
        })?;
        let method = operation.parameters.get("method").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.command.request requires method".into(),
            ))
        })?;
        let body = operation.parameters.get("body").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.command.request requires body".into(),
            ))
        })?;
        let content_type = operation.parameters.get("content_type").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.command.request requires content_type".into(),
            ))
        })?;
        if !valid_request_parameters(&operation.parameters)
            || method != "POST"
            || content_type.contains(['\r', '\n', '\0'])
            || secret.is_empty()
            || secret
                .iter()
                .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'"' | b'\\'))
        {
            return Err(CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.command.request parameters or credential are unsupported".into(),
            )));
        }
        let sandbox_name = effect_sandbox_name(operation_request_id)?;
        let cpu = sandbox.vcpus.to_string();
        let memory = format!("{}Mi", sandbox.memory_mib);
        let create = self.openshell(
            sandbox,
            &[
                "sandbox",
                "create",
                "--name",
                &sandbox_name,
                "--from",
                &sandbox.base_image,
                "--policy",
                path_text(&sandbox.policy_path)?,
                "--no-auto-providers",
                "--cpu",
                &cpu,
                "--memory",
                &memory,
                "--no-tty",
                "--",
                "true",
            ],
            None,
            deadline,
        )?;
        if let Err(error) = require_success("Effect Sandbox creation", create) {
            let _ = self.delete_sandbox(sandbox, &sandbox_name);
            return Err(CredentialEffectError::Failed(error));
        }
        let execution = (|| -> Result<Value, CredentialEffectError> {
            require_success(
                "Effect Sandbox adapter upload",
                self.openshell(
                    sandbox,
                    &[
                        "sandbox",
                        "upload",
                        &sandbox_name,
                        path_text(&sandbox.adapter_binary)?,
                        "/sandbox/effect-adapter",
                    ],
                    None,
                    deadline,
                )?,
            )?;
            require_success(
                "Effect Sandbox adapter permission",
                self.openshell(
                    sandbox,
                    &[
                        "sandbox",
                        "exec",
                        "-n",
                        &sandbox_name,
                        "--no-tty",
                        "--",
                        "chmod",
                        "700",
                        "/sandbox/effect-adapter",
                    ],
                    None,
                    deadline,
                )?,
            )?;
            let preflight = "set -eu; printf tyrion-effect-containment-probe; test \"$(cat /sys/fs/cgroup/pids.max)\" = 256; test \"$(getconf _NPROCESSORS_ONLN)\" = 2; test ! -e /var/run/docker.sock; test ! -e /run/containerd/containerd.sock; test ! -e /home/sandbox/.ssh; test ! -e /home/sandbox/.aws; test ! -e /home/sandbox/.config/gh; test -z \"${OPENAI_API_KEY:-}${ANTHROPIC_API_KEY:-}${AWS_ACCESS_KEY_ID:-}${GH_TOKEN:-}${GITHUB_TOKEN:-}${SSH_AUTH_SOCK:-}\"";
            require_success(
                "Effect Sandbox containment preflight",
                self.openshell(
                    sandbox,
                    &[
                        "sandbox",
                        "exec",
                        "-n",
                        &sandbox_name,
                        "--no-tty",
                        "--",
                        "sh",
                        "-c",
                        preflight,
                    ],
                    None,
                    deadline,
                )?,
            )?;
            let version = require_success(
                "Effect Sandbox adapter version",
                self.openshell(
                    sandbox,
                    &[
                        "sandbox",
                        "exec",
                        "-n",
                        &sandbox_name,
                        "--no-tty",
                        "--",
                        "/sandbox/effect-adapter",
                        "--version",
                    ],
                    None,
                    deadline,
                )?,
            )?;
            if String::from_utf8_lossy(&version.stdout).trim() != sandbox.adapter_version {
                return Err(CredentialEffectError::Failed(TyrionError::ControlDenied(
                    "Effect Sandbox adapter version does not match its pin".into(),
                )));
            }
            let mut input = Zeroizing::new(Vec::with_capacity(
                secret.len() + binding.resolved_url.len() + content_type.len() + body.len() + 4,
            ));
            input.extend_from_slice(secret);
            input.push(b'\n');
            input.extend_from_slice(binding.resolved_url.as_bytes());
            input.push(b'\n');
            input.extend_from_slice(content_type.as_bytes());
            input.push(b'\n');
            input.extend_from_slice(body.as_bytes());
            let mut output = match self.openshell_bounded(
                sandbox,
                &[
                    "sandbox",
                    "exec",
                    "-n",
                    &sandbox_name,
                    "--no-tty",
                    "--",
                    "/sandbox/effect-adapter",
                    "--execute-stdin",
                ],
                Some(&input),
                deadline
                    .remaining()
                    .map_err(CredentialEffectError::Failed)?,
                operation.limits.max_output_bytes,
            ) {
                Ok(output) => output,
                Err(CredentialEffectError::Failed(error)) => {
                    return Err(CredentialEffectError::Uncertain {
                        error,
                        receipt: uncertain_delivery_receipt(operation, binding, deadline),
                    });
                }
                Err(error) => return Err(error),
            };
            if leave_started_before_cleanup {
                return Err(CredentialEffectError::LeaveStartedAfterEffect);
            }
            let mut logs = match self.openshell(
                sandbox,
                &["logs", &sandbox_name, "-n", "300"],
                None,
                deadline,
            ) {
                Ok(logs) => logs,
                Err(CredentialEffectError::Failed(error)) => {
                    return Err(CredentialEffectError::Uncertain {
                        error,
                        receipt: uncertain_delivery_receipt(operation, binding, deadline),
                    });
                }
                Err(error) => return Err(error),
            };
            let leaked = contains_bytes(&output.stdout, secret)
                || contains_bytes(&output.stderr, secret)
                || contains_bytes(&logs.stdout, secret)
                || contains_bytes(&logs.stderr, secret);
            let returned_bytes = output.stdout.len().saturating_add(output.stderr.len()) as u64;
            let marker = b"\nTYRION_HTTP_STATUS:";
            let status = output
                .stdout
                .windows(marker.len())
                .rposition(|window| window == marker)
                .and_then(|position| output.stdout.get(position + marker.len()..))
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .and_then(|value| value.trim().parse::<u16>().ok());
            let receipt = serde_json::json!({
                "status": if status.is_some_and(|status| (200..300).contains(&status)) { "confirmed" } else { "uncertain" },
                "operation": operation.operation,
                "destination": binding.destination,
                "target": operation.target,
                "http_status": status,
                "response_sha256": format!("{:x}", Sha256::digest(&output.stdout)),
                "returned_bytes": returned_bytes,
                "duration_millis": deadline.started.elapsed().as_millis(),
                "credential_delivery": "effect_sandbox_stdin",
                "credential_store": "macos_keychain",
                "response_body_retained": false,
                "secret_leak_detected": leaked,
                "secret_material_retained": false,
                "sandbox_fresh": true,
                "sandbox_non_agentic": true,
                "sandbox_policy_sha256": sandbox.policy_sha256,
                "adapter_sha256": sandbox.adapter_sha256,
            });
            let output_success = output.status.success();
            let logs_success = logs.status.success();
            let containment_attested = String::from_utf8_lossy(&logs.stdout)
                .contains("Landlock ruleset built")
                && String::from_utf8_lossy(&logs.stdout).contains("network policy enforced");
            output.stdout.zeroize();
            output.stderr.zeroize();
            logs.stdout.zeroize();
            logs.stderr.zeroize();
            if leaked
                || returned_bytes > operation.limits.max_output_bytes
                || !output_success
                || status.is_none_or(|status| !(200..300).contains(&status))
                || !logs_success
                || !containment_attested
            {
                return Err(CredentialEffectError::Uncertain {
                    error: TyrionError::InvalidRequest(
                        "Effect Sandbox execution or containment attestation failed".into(),
                    ),
                    receipt,
                });
            }
            Ok(receipt)
        })();
        if matches!(
            execution,
            Err(CredentialEffectError::LeaveStartedAfterEffect)
        ) {
            return execution;
        }
        let cleanup = self.delete_sandbox(sandbox, &sandbox_name);
        match (execution, cleanup) {
            (Ok(mut receipt), Ok(())) => {
                receipt["sandbox_destroyed"] = Value::Bool(true);
                receipt["descendants_terminated"] = Value::Bool(true);
                Ok(receipt)
            }
            (Err(CredentialEffectError::Uncertain { error, mut receipt }), Ok(())) => {
                receipt["sandbox_destroyed"] = Value::Bool(true);
                receipt["descendants_terminated"] = Value::Bool(true);
                Err(CredentialEffectError::Uncertain { error, receipt })
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(receipt), Err(error)) => Err(CredentialEffectError::Uncertain {
                error,
                receipt: serde_json::json!({
                    "status": "uncertain",
                    "external_response": receipt,
                    "sandbox_destroyed": false,
                    "descendants_terminated": false,
                    "secret_material_retained": false,
                    "requirement": "Delete the exact Effect Sandbox before resuming."
                }),
            }),
            (Err(_), Err(error)) => Err(CredentialEffectError::Uncertain {
                error,
                receipt: serde_json::json!({
                    "status": "uncertain",
                    "effect_may_have_occurred": true,
                    "sandbox_destroyed": false,
                    "descendants_terminated": false,
                    "secret_material_retained": false,
                    "requirement": "Delete the exact Effect Sandbox before resuming."
                }),
            }),
        }
    }

    fn openshell(
        &self,
        sandbox: &EffectSandboxConfig,
        arguments: &[&str],
        input: Option<&[u8]>,
        deadline: CredentialExecutionDeadline,
    ) -> Result<std::process::Output, CredentialEffectError> {
        self.openshell_bounded(
            sandbox,
            arguments,
            input,
            deadline
                .remaining()
                .map_err(CredentialEffectError::Failed)?
                .min(Duration::from_secs(15)),
            1024 * 1024,
        )
    }

    fn openshell_bounded(
        &self,
        sandbox: &EffectSandboxConfig,
        arguments: &[&str],
        input: Option<&[u8]>,
        max_duration: Duration,
        max_output_bytes: u64,
    ) -> Result<std::process::Output, CredentialEffectError> {
        let mut command = Command::new(&sandbox.openshell_binary);
        command
            .args(arguments)
            .env_clear()
            .env("XDG_CONFIG_HOME", &sandbox.openshell_config_home)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_bounded_command(&mut command, input, max_duration, max_output_bytes)
            .map_err(CredentialEffectError::Failed)
    }

    fn delete_sandbox(&self, sandbox: &EffectSandboxConfig, name: &str) -> Result<(), TyrionError> {
        validate_hash(&sandbox.openshell_binary, &sandbox.openshell_sha256)?;
        let mut command = Command::new(&sandbox.openshell_binary);
        command
            .args(["sandbox", "delete", name])
            .env_clear()
            .env("XDG_CONFIG_HOME", &sandbox.openshell_config_home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_bounded_command(&mut command, None, Duration::from_secs(15), 1024 * 1024)?;
        require_success("Effect Sandbox deletion", output)?;
        Ok(())
    }

    fn run_curl(
        &self,
        operation: &OperationRequest,
        binding: &CredentialEffectBinding,
        secret: &[u8],
        operation_request_id: &str,
        deadline: CredentialExecutionDeadline,
        process_started: &mut dyn FnMut(u32, &str) -> Result<(), TyrionError>,
    ) -> Result<Value, CredentialEffectError> {
        let method = operation.parameters.get("method").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.http.request requires method".into(),
            ))
        })?;
        let body = operation.parameters.get("body").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.http.request requires body".into(),
            ))
        })?;
        let content_type = operation.parameters.get("content_type").ok_or_else(|| {
            CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.http.request requires content_type".into(),
            ))
        })?;
        if !valid_request_parameters(&operation.parameters)
            || method != "POST"
            || content_type.contains(['\r', '\n', '\0'])
            || secret.is_empty()
            || secret
                .iter()
                .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'"' | b'\\'))
        {
            return Err(CredentialEffectError::Failed(TyrionError::InvalidRequest(
                "credential.http.request parameters or bearer credential are unsupported".into(),
            )));
        }
        let mut credential_config = Zeroizing::new(Vec::with_capacity(secret.len() + 40));
        credential_config.extend_from_slice(b"header = \"Authorization: Bearer ");
        credential_config.extend_from_slice(secret);
        credential_config.extend_from_slice(b"\"\n");
        let max_time = operation.limits.max_duration_seconds.to_string();
        let max_output = operation.limits.max_output_bytes.to_string();
        let process_marker = broker_process_marker(operation_request_id)?;
        let write_out =
            format!("\nTYRION_PROCESS_MARKER:{process_marker}\nTYRION_HTTP_STATUS:%{{http_code}}");
        let mut command = Command::new(&self.config.broker.curl_binary);
        command
            .args([
                "--disable",
                "--config",
                "-",
                "--silent",
                "--show-error",
                "--max-time",
                &max_time,
                "--max-filesize",
                &max_output,
                "--max-redirs",
                "0",
                "--proto",
                "=http,https",
                "--request",
                method,
                "--user-agent",
                "",
                "--header",
                &format!("Content-Type: {content_type}"),
                "--data-binary",
                body,
                "--write-out",
                &write_out,
                &binding.resolved_url,
            ])
            .env_clear()
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut output = run_bounded_command_after_spawn(
            &mut command,
            Some(&credential_config),
            operation.limits.max_output_bytes,
            true,
            |process_id| {
                process_started(process_id, &process_marker)?;
                deadline.instant_deadline()
            },
        )
        .map_err(|error| CredentialEffectError::Uncertain {
            error,
            receipt: uncertain_delivery_receipt(operation, binding, deadline),
        })?;
        let leaked =
            contains_bytes(&output.stdout, secret) || contains_bytes(&output.stderr, secret);
        let output_bytes = output.stdout.len().saturating_add(output.stderr.len()) as u64;
        let response_sha256 = format!("{:x}", Sha256::digest(&output.stdout));
        let marker = b"\nTYRION_HTTP_STATUS:";
        let status = output
            .stdout
            .windows(marker.len())
            .rposition(|window| window == marker)
            .and_then(|position| output.stdout.get(position + marker.len()..))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|value| value.trim().parse::<u16>().ok());
        let receipt = serde_json::json!({
            "status": if status.is_some_and(|status| (200..300).contains(&status)) { "confirmed" } else { "uncertain" },
            "operation": operation.operation,
            "destination": binding.destination,
            "target": operation.target,
            "http_status": status,
            "response_sha256": response_sha256,
            "returned_bytes": output_bytes,
            "duration_millis": deadline.started.elapsed().as_millis(),
            "credential_delivery": "brokered_stdin",
            "credential_store": "macos_keychain",
            "broker_process_contained": true,
            "descendants_terminated": true,
            "response_body_retained": false,
            "secret_leak_detected": leaked,
            "secret_material_retained": false,
        });
        let output_success = output.status.success();
        output.stdout.zeroize();
        output.stderr.zeroize();
        if leaked
            || output_bytes > operation.limits.max_output_bytes
            || !output_success
            || status.is_none_or(|status| !(200..300).contains(&status))
        {
            return Err(CredentialEffectError::Uncertain {
                error: TyrionError::InvalidRequest(
                    "credentialed HTTP effect did not return a bounded successful response".into(),
                ),
                receipt,
            });
        }
        Ok(receipt)
    }

    fn read_secret(&self, credential_reference: &str) -> Result<Zeroizing<Vec<u8>>, TyrionError> {
        let output = self.keychain_command("find-generic-password", credential_reference, true)?;
        if !output.status.success() {
            return Err(TyrionError::ControlDenied(
                "the credential is unavailable from the operating-system store".into(),
            ));
        }
        let mut secret = Zeroizing::new(output.stdout);
        if secret.last() == Some(&b'\n') {
            secret.pop();
        }
        if secret.last() == Some(&b'\r') {
            secret.pop();
        }
        if secret.is_empty() {
            return Err(TyrionError::ControlDenied(
                "the operating-system credential store returned an empty credential".into(),
            ));
        }
        Ok(secret)
    }

    fn revoke_and_verify(&self, credential_reference: &str) -> Result<(), TyrionError> {
        let deleted =
            self.keychain_command("delete-generic-password", credential_reference, false)?;
        let found = self.keychain_command("find-generic-password", credential_reference, false)?;
        if found.status.success() {
            return Err(TyrionError::ControlDenied(
                "the revoked credential remains available".into(),
            ));
        }
        if found.status.code() == Some(44) {
            Ok(())
        } else if deleted.status.success() {
            Err(TyrionError::ControlDenied(
                "credential deletion completed but absence could not be verified".into(),
            ))
        } else {
            Err(TyrionError::ControlDenied(
                "credential revocation and absence verification both failed".into(),
            ))
        }
    }

    fn contain_broker_process(
        &self,
        recorded_process_id: Option<u32>,
        marker: &str,
    ) -> Result<(), TyrionError> {
        let Some(process_id) = recorded_process_id else {
            // Credential bytes are written only after this identity is durably registered.
            return Ok(());
        };
        let Some(identity) = process_identity(process_id)? else {
            return if process_group_exists(process_id)? {
                Err(TyrionError::ControlDenied(
                    "credential broker leader is absent while its process group remains; manual containment is required"
                        .into(),
                ))
            } else {
                Ok(())
            };
        };
        let expected_marker = format!("TYRION_PROCESS_MARKER:{marker}");
        let normalized_command = identity.command.replace(r"\012", "\n");
        let marker_matches = normalized_command
            .split_whitespace()
            .filter(|argument| *argument == expected_marker)
            .count()
            == 1;
        if identity.process_group_id != process_id || !marker_matches {
            return Err(TyrionError::ControlDenied(
                "recorded credential broker PID was reused and cannot be killed safely".into(),
            ));
        }
        terminate_process_group(process_id)
    }

    fn revalidate_broker(&self) -> Result<(), TyrionError> {
        validate_hash(
            &self.config.keychain.security_binary,
            &self.config.keychain.security_sha256,
        )?;
        validate_hash(
            &self.config.broker.curl_binary,
            &self.config.broker.curl_sha256,
        )?;
        Ok(())
    }

    fn keychain_command(
        &self,
        subcommand: &str,
        credential_reference: &str,
        reveal: bool,
    ) -> Result<std::process::Output, TyrionError> {
        validate_reference(credential_reference)?;
        validate_hash(
            &self.config.keychain.security_binary,
            &self.config.keychain.security_sha256,
        )?;
        let mut command = Command::new(&self.config.keychain.security_binary);
        command
            .arg(subcommand)
            .args(["-a", credential_reference])
            .args(["-s", &self.config.keychain.service]);
        if reveal {
            command.arg("-w");
        }
        command
            .arg(&self.config.keychain.keychain_path)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_bounded_command(&mut command, None, Duration::from_secs(15), 1024 * 1024)
    }
}

fn validate_hash(path: &Path, expected: &str) -> Result<(), TyrionError> {
    let actual = format!("{:x}", Sha256::digest(fs::read(path)?));
    if actual != expected {
        return Err(TyrionError::ControlDenied(format!(
            "credential runtime executable {} does not match its SHA-256 pin",
            path.display()
        )));
    }
    Ok(())
}

fn validate_effect_sandbox(
    sandbox: &EffectSandboxConfig,
    broker: &BrokerConfig,
) -> Result<(), TyrionError> {
    const BASE_IMAGE: &str = "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e";
    const SOURCE_REVISION: &str = "dd2b4e3bc0688bdd59f90030f7c1d52511d6e354";
    const SOURCE_PATCH_SHA256: &str =
        "6452fbe2836ffbe43e0e73c813db5dc5dda7ee70537b7033fc5429573160e402";
    validate_hash(&sandbox.openshell_binary, &sandbox.openshell_sha256)?;
    validate_hash(&sandbox.policy_path, &sandbox.policy_sha256)?;
    validate_hash(&sandbox.gateway_config_path, &sandbox.gateway_config_sha256)?;
    validate_hash(&sandbox.kernel_config_path, &sandbox.kernel_config_sha256)?;
    validate_hash(&sandbox.source_patch_path, &sandbox.source_patch_sha256)?;
    validate_hash(&sandbox.adapter_binary, &sandbox.adapter_sha256)?;
    for artifact in &sandbox.runtime_artifacts {
        validate_hash(&artifact.path, &artifact.sha256)?;
    }
    if sandbox.openshell_version != "openshell 0.0.104"
        || sandbox.base_image != BASE_IMAGE
        || sandbox.source_revision != SOURCE_REVISION
        || sandbox.source_patch_sha256 != SOURCE_PATCH_SHA256
        || sandbox.vcpus != 2
        || sandbox.memory_mib != 2048
        || sandbox.overlay_disk_mib != 4096
        || sandbox.max_processes != 256
        || sandbox.runtime_artifacts.is_empty()
        || sandbox.adapter_version.trim().is_empty()
        || !sandbox.openshell_config_home.is_dir()
    {
        return Err(TyrionError::InvalidRequest(
            "Effect Sandbox configuration does not match the bounded one-shot profile".into(),
        ));
    }
    let gateway = fs::read_to_string(&sandbox.gateway_config_path)?;
    for required in [
        "compute_drivers = [\"vm\"]",
        "enabled = true",
        "vcpus = 2",
        "mem_mib = 2048",
        "overlay_disk_mib = 4096",
    ] {
        if !gateway.lines().any(|line| line.trim() == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "Effect Sandbox gateway configuration is missing {required}"
            )));
        }
    }
    let kernel = fs::read_to_string(&sandbox.kernel_config_path)?;
    for required in [
        "CONFIG_SECURITY=y",
        "CONFIG_SECURITY_LANDLOCK=y",
        "CONFIG_LSM=\"landlock,lockdown,yama,integrity\"",
        "CONFIG_CGROUP_PIDS=y",
        "CONFIG_SECCOMP_FILTER=y",
    ] {
        if !kernel.lines().any(|line| line == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "Effect Sandbox kernel configuration is missing {required}"
            )));
        }
    }
    let destination = broker
        .destinations
        .get(&sandbox.destination)
        .ok_or_else(|| {
            TyrionError::InvalidRequest(
                "Effect Sandbox destination is absent from the credential broker".into(),
            )
        })?;
    let origin = parse_destination_origin(destination).ok_or_else(|| {
        TyrionError::InvalidRequest("invalid Effect Sandbox destination origin".into())
    })?;
    let port = origin.port.ok_or_else(|| {
        TyrionError::InvalidRequest("Effect Sandbox destination requires an exact port".into())
    })?;
    let policy = fs::read_to_string(&sandbox.policy_path)?;
    for required in [
        "include_workdir: false".to_owned(),
        "compatibility: hard_requirement".to_owned(),
        "run_as_user: sandbox".to_owned(),
        format!("- host: {}", origin.host),
        format!("port: {port}"),
        "enforcement: enforce".to_owned(),
        "- path: /sandbox/effect-adapter".to_owned(),
        "- path: /usr/bin/curl".to_owned(),
    ] {
        if !policy.lines().any(|line| line.trim() == required) {
            return Err(TyrionError::InvalidRequest(format!(
                "Effect Sandbox policy is missing {required}"
            )));
        }
    }
    let endpoint_hosts = policy
        .lines()
        .filter(|line| line.trim().starts_with("- host:"))
        .collect::<Vec<_>>();
    let endpoint_ports = policy
        .lines()
        .filter(|line| line.trim().starts_with("port:"))
        .collect::<Vec<_>>();
    let executable_paths = policy
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- path: "))
        .filter(|path| *path == "/sandbox/effect-adapter" || *path == "/usr/bin/curl")
        .collect::<Vec<_>>();
    let allowed_paths = [
        "/usr",
        "/lib",
        "/lib64",
        "/proc",
        "/sys/fs/cgroup",
        "/dev/urandom",
        "/etc",
        "/sandbox",
        "/tmp",
        "/dev/null",
        "/sandbox/effect-adapter",
        "/usr/bin/curl",
    ];
    let unexpected_path = policy.lines().find_map(|line| {
        let value = line.trim().strip_prefix("- ")?;
        (value.starts_with('/') && !allowed_paths.contains(&value)).then_some(value)
    });
    if endpoint_hosts.len() != 1
        || endpoint_ports.len() != 1
        || executable_paths.len() != 2
        || unexpected_path.is_some()
    {
        return Err(TyrionError::InvalidRequest(
            "Effect Sandbox policy grants resources beyond the exact one-shot profile".into(),
        ));
    }
    let mut command = Command::new(&sandbox.openshell_binary);
    command
        .arg("--version")
        .env_clear()
        .env("XDG_CONFIG_HOME", &sandbox.openshell_config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let version = run_bounded_command(&mut command, None, Duration::from_secs(15), 1024 * 1024)?;
    let version = require_success("OpenShell version probe", version)?;
    if String::from_utf8_lossy(&version.stdout).trim() != sandbox.openshell_version {
        return Err(TyrionError::ControlDenied(
            "OpenShell version does not match the Effect Sandbox pin".into(),
        ));
    }
    Ok(())
}

fn require_success(
    label: &str,
    output: std::process::Output,
) -> Result<std::process::Output, TyrionError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(TyrionError::InvalidRequest(format!(
            "{label} failed with status {}",
            output.status
        )))
    }
}

fn path_text(path: &Path) -> Result<&str, TyrionError> {
    path.to_str().ok_or_else(|| {
        TyrionError::InvalidRequest("credential runtime paths must contain valid UTF-8".into())
    })
}

fn validate_destination(name: &str, destination: &str) -> Result<(), TyrionError> {
    if name.trim().is_empty()
        || name.contains('\0')
        || parse_destination_origin(destination).is_none()
    {
        return Err(TyrionError::InvalidRequest(
            "credential destinations must be exact HTTPS origins or loopback test origins".into(),
        ));
    }
    Ok(())
}

struct DestinationOrigin<'a> {
    host: &'a str,
    port: Option<u16>,
}

fn parse_destination_origin(destination: &str) -> Option<DestinationOrigin<'_>> {
    let (scheme, authority) = if let Some(authority) = destination.strip_prefix("https://") {
        ("https", authority)
    } else if let Some(authority) = destination.strip_prefix("http://") {
        ("http", authority)
    } else {
        return None;
    };
    if authority.is_empty()
        || authority.contains(['/', '\\', '@', '?', '#', '\0'])
        || authority.bytes().any(|byte| byte.is_ascii_whitespace())
        || authority.matches(':').count() > 1
    {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
        None => (authority, None),
    };
    if host.is_empty()
        || host.starts_with(['.', '-'])
        || host.ends_with(['.', '-'])
        || host.contains("..")
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        || port == Some(0)
        || (scheme == "http" && (host != "127.0.0.1" || port.is_none()))
    {
        return None;
    }
    Some(DestinationOrigin { host, port })
}

fn validate_reference(reference: &str) -> Result<(), TyrionError> {
    if reference.trim().is_empty()
        || reference.len() > 128
        || !reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(TyrionError::InvalidRequest(
            "credential references must be bounded opaque identifiers".into(),
        ));
    }
    Ok(())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn effect_sandbox_name(operation_request_id: &str) -> Result<String, TyrionError> {
    if operation_request_id.len() != 36
        || !operation_request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(TyrionError::InvalidRequest(
            "Effect Sandbox identity requires a canonical operation UUID".into(),
        ));
    }
    Ok(format!("tyrion-e-{operation_request_id}"))
}

fn broker_process_marker(operation_request_id: &str) -> Result<String, TyrionError> {
    if operation_request_id.len() != 36
        || !operation_request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(TyrionError::InvalidRequest(
            "credential broker identity requires a canonical operation UUID".into(),
        ));
    }
    Ok(format!("tyrion-effect-{operation_request_id}"))
}

struct ProcessIdentity {
    process_group_id: u32,
    command: String,
}

fn process_identity(process_id: u32) -> Result<Option<ProcessIdentity>, TyrionError> {
    let mut command = Command::new("/bin/ps");
    command
        .args([
            "-ww",
            "-p",
            &process_id.to_string(),
            "-o",
            "pid=,pgid=,command=",
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_command(&mut command, None, Duration::from_secs(5), 64 * 1024)?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let mut fields = line.split_whitespace();
    let observed_process_id = fields.next().and_then(|value| value.parse::<u32>().ok());
    let Some(process_group_id) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
        return Err(TyrionError::ControlDenied(
            "credential broker process identity could not be parsed safely".into(),
        ));
    };
    if observed_process_id != Some(process_id) {
        return Err(TyrionError::ControlDenied(
            "credential broker process identity changed during inspection".into(),
        ));
    }
    Ok(Some(ProcessIdentity {
        process_group_id,
        command: line.into_owned(),
    }))
}

fn process_group_exists(process_id: u32) -> Result<bool, TyrionError> {
    let process_group = i32::try_from(process_id).map_err(|_| {
        TyrionError::ControlDenied("credential broker process-group identity is invalid".into())
    })?;
    // SAFETY: signal zero checks only the existence of the validated process group.
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error.into()),
    }
}

fn signal_process_group(process_id: u32, signal: i32) -> Result<(), TyrionError> {
    let process_group = i32::try_from(process_id).map_err(|_| {
        TyrionError::ControlDenied("credential broker process-group identity is invalid".into())
    })?;
    // SAFETY: a negative, validated child PID targets only the child's process group.
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error.into())
    }
}

fn terminate_process_group(process_id: u32) -> Result<(), TyrionError> {
    if !process_group_exists(process_id)? {
        return Ok(());
    }
    signal_process_group(process_id, libc::SIGTERM)?;
    for _ in 0..10 {
        if !process_group_exists(process_id)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    signal_process_group(process_id, libc::SIGKILL)?;
    for _ in 0..10 {
        if !process_group_exists(process_id)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(TyrionError::ControlDenied(
        "the credential broker process group remains live after forced termination".into(),
    ))
}

fn uncertain_delivery_receipt(
    operation: &OperationRequest,
    binding: &CredentialEffectBinding,
    deadline: CredentialExecutionDeadline,
) -> Value {
    serde_json::json!({
        "status": "uncertain",
        "operation": operation.operation,
        "destination": binding.destination,
        "target": operation.target,
        "duration_millis": deadline.started.elapsed().as_millis(),
        "effect_may_have_occurred": true,
        "response_body_retained": false,
        "secret_material_retained": false,
        "requirement": "Reconcile the exact external target read-only before resuming.",
    })
}

fn valid_request_parameters(parameters: &BTreeMap<String, String>) -> bool {
    let allowed = [
        "body",
        "content_type",
        "method",
        "reconciliation_target",
        "confirmed_reconciliation_sha256",
        "not_applied_reconciliation_sha256",
    ];
    parameters.keys().all(|key| allowed.contains(&key.as_str()))
        && parameters.len() == 6
        && parameters.contains_key("confirmed_reconciliation_sha256")
        && parameters.contains_key("not_applied_reconciliation_sha256")
}

fn run_bounded_command(
    command: &mut Command,
    input: Option<&[u8]>,
    max_duration: Duration,
    max_output_bytes: u64,
) -> Result<Output, TyrionError> {
    let deadline = Instant::now().checked_add(max_duration).ok_or_else(|| {
        TyrionError::InvalidRequest("bounded command deadline exceeds Instant".into())
    })?;
    run_bounded_command_after_spawn(command, input, max_output_bytes, false, |_| Ok(deadline))
}

fn run_bounded_command_after_spawn(
    command: &mut Command,
    input: Option<&[u8]>,
    max_output_bytes: u64,
    terminate_group: bool,
    mut after_spawn: impl FnMut(u32) -> Result<Instant, TyrionError>,
) -> Result<Output, TyrionError> {
    let mut child = command.spawn()?;
    let process_id = child.id();
    let deadline = match after_spawn(process_id) {
        Ok(deadline) => deadline,
        Err(error) => {
            return Err(cleanup_spawned_command(
                &mut child,
                process_id,
                terminate_group,
                error,
            ));
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let error = TyrionError::InvalidRequest("bounded command stdout is unavailable".into());
        return Err(cleanup_spawned_command(
            &mut child,
            process_id,
            terminate_group,
            error,
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        let error = TyrionError::InvalidRequest("bounded command stderr is unavailable".into());
        return Err(cleanup_spawned_command(
            &mut child,
            process_id,
            terminate_group,
            error,
        ));
    };
    let limit = usize::try_from(max_output_bytes)
        .unwrap_or(usize::MAX.saturating_sub(1))
        .saturating_add(1);
    let stdout_reader = thread::spawn(move || read_bounded(stdout, limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, limit));
    let input_writer = if let Some(input) = input {
        let mut input = Zeroizing::new(input.to_vec());
        let Some(mut stdin) = child.stdin.take() else {
            let error = TyrionError::InvalidRequest("bounded command stdin is unavailable".into());
            return Err(cleanup_spawned_command(
                &mut child,
                process_id,
                terminate_group,
                error,
            ));
        };
        Some(thread::spawn(move || {
            if Instant::now() >= deadline {
                input.clear();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "bounded command input deadline expired before delivery",
                ));
            }
            let result = stdin.write_all(&input);
            input.clear();
            result
        }))
    } else {
        None
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                return Err(cleanup_spawned_command(
                    &mut child,
                    process_id,
                    terminate_group,
                    error.into(),
                ));
            }
        }
        if Instant::now() >= deadline {
            let error = TyrionError::InvalidRequest(
                "credential effect exceeded its exact duration limit".into(),
            );
            return Err(cleanup_spawned_command(
                &mut child,
                process_id,
                terminate_group,
                error,
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    if terminate_group {
        terminate_process_group(process_id)?;
    }
    if let Some(writer) = input_writer {
        writer.join().map_err(|_| {
            TyrionError::InvalidRequest("credential effect input writer panicked".into())
        })??;
    }
    let stdout = join_bounded_reader(stdout_reader)?;
    let stderr = join_bounded_reader(stderr_reader)?;
    if stdout.len().saturating_add(stderr.len()) as u64 > max_output_bytes {
        return Err(TyrionError::InvalidRequest(
            "credential effect exceeded its exact output limit".into(),
        ));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn cleanup_spawned_command(
    child: &mut std::process::Child,
    process_id: u32,
    terminate_group: bool,
    original_error: TyrionError,
) -> TyrionError {
    let cleanup = if terminate_group {
        let _ = signal_process_group(process_id, libc::SIGKILL);
        let _ = child.kill();
        let wait = child.wait().map(|_| ()).map_err(TyrionError::from);
        let containment = terminate_process_group(process_id);
        containment.and(wait)
    } else {
        let _ = child.kill();
        child.wait().map(|_| ()).map_err(TyrionError::from)
    };
    cleanup.err().unwrap_or(original_error)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_bounded_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, TyrionError> {
    reader
        .join()
        .map_err(|_| TyrionError::InvalidRequest("credential output reader panicked".into()))?
        .map_err(TyrionError::Io)
}

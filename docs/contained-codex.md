# Contained Codex Git assignments

Tyrion supports one production Git assignment profile: Codex CLI `0.147.0` inside the repaired OpenShell `0.0.104` MicroVM boundary. The profile pins OpenShell source revision `dd2b4e3bc0688bdd59f90030f7c1d52511d6e354`, the tracked repair patch and its digest, the base image digest, the hard Landlock policy, the repaired kernel configuration, every runtime artifact, and the Linux Codex binary. Startup rejects missing or hash-mismatched artifacts. After transfer, the MicroVM itself verifies the guest-only Codex binary version before executing it.

The gateway must use the VM driver with mTLS, 2 vCPUs, 2 GiB memory, a 4 GiB overlay, and the repaired guest's 256-process child cgroup. The checked policy at [`runtime/openshell/hard-landlock-policy.yaml`](../runtime/openshell/hard-landlock-policy.yaml) allows only the uploaded `/sandbox/codex` binary to reach the pinned OpenAI hosts. The explicitly selected provider supplies the matching endpoint-bound credential resolver; all other egress remains denied.

## Build the Repaired Runtime

Stock OpenShell `0.0.104` is not the supported artifact. Its kernel fragment declares Landlock without enabling Linux security or adding Landlock to the active LSM list, and its guest init does not create the required 256-process cgroup. Apply the exact tracked patch before building:

```sh
git clone https://github.com/NVIDIA/OpenShell.git openshell-0.0.104
cd openshell-0.0.104
git checkout dd2b4e3bc0688bdd59f90030f7c1d52511d6e354
git apply /absolute/path/to/tyrion/runtime/openshell/repaired-v0.0.104.patch
git apply --reverse --check /absolute/path/to/tyrion/runtime/openshell/repaired-v0.0.104.patch
```

Build the ARM64 Linux kernel firmware with the patched `tasks/scripts/vm/build-libkrun.sh`, then build the macOS libraries with `tasks/scripts/vm/build-libkrun-macos.sh --kernel-dir <linux-output>`. Build the supervisor bundle and `openshell-driver-vm` from that same checkout. The final kernel configuration must include these exact lines:

```text
CONFIG_SECURITY=y
CONFIG_SECURITY_LANDLOCK=y
CONFIG_LSM="landlock,lockdown,yama,integrity"
CONFIG_CGROUP_PIDS=y
CONFIG_SECCOMP_FILTER=y
```

The Apple Silicon artifacts used for the successful attestation are pinned in [`runtime/openshell/codex-worker.example.json`](../runtime/openshell/codex-worker.example.json). Place the repaired driver in the gateway `driver_dir` under the exact name `openshell-driver-vm`. Delete or move the existing `sandbox-bootstrap-rootfs-ext4-v3-openshell-0.0.104-*` cache before first launch because OpenShell's cache identity does not include the guest-init contents. The next sandbox creation must rebuild that cache from the repaired driver.

The gateway configuration must contain these values in addition to valid mTLS and guest TLS paths:

```toml
[openshell.gateway]
compute_drivers = ["vm"]
disable_tls = false

[openshell.gateway.mtls_auth]
enabled = true

[openshell.drivers.vm]
vcpus = 2
mem_mib = 2048
overlay_disk_mib = 4096
```

## Configure the Worker

Copy [`runtime/openshell/codex-worker.example.json`](../runtime/openshell/codex-worker.example.json), replace every absolute path, and record the actual SHA-256 of the Linux aarch64 Codex binary. The binary must report `codex-cli 0.147.0`.

`openshell_provider` names an existing OpenShell Codex provider. Load it from a current host Codex login without placing a token on the command line:

```sh
CODEX_AUTH_ACCESS_TOKEN="$(jq -r '.tokens.access_token' "$HOME/.codex/auth.json")" \
CODEX_AUTH_REFRESH_TOKEN="$(jq -r '.tokens.refresh_token' "$HOME/.codex/auth.json")" \
CODEX_AUTH_ACCOUNT_ID="$(jq -r '.tokens.account_id' "$HOME/.codex/auth.json")" \
CODEX_AUTH_ID_TOKEN="$(jq -r '.tokens.id_token' "$HOME/.codex/auth.json")" \
  openshell provider create --name tyrion-codex --type codex \
  --credential CODEX_AUTH_ACCESS_TOKEN \
  --credential CODEX_AUTH_REFRESH_TOKEN \
  --credential CODEX_AUTH_ACCOUNT_ID \
  --credential CODEX_AUTH_ID_TOKEN
```

Use `provider update` with the same credential flags when the host login rotates. The provider exposes only revision-scoped `openshell:resolve:env:*` placeholders inside the VM. Tyrion rejects raw values before launch and preserves those exact injected placeholders in the disposable Codex home. Codex receives only the OpenShell proxy and CA variables from the ambient environment. Its local ID-token surrogate is parseable but contains no credential, and the proxy replaces access and account placeholders only on authorized OpenAI requests. No raw model or repository credential enters the Attempt.

Start the already configured repaired gateway with telemetry disabled, then start Tyrion:

```sh
OPENSHELL_TELEMETRY_ENABLED=false openshell-gateway --config /absolute/path/to/gateway.toml

target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config /absolute/path/to/codex-worker.json
```

## Propose a Git Commission

The immutable base must be a full Git object ID. The repository path must appear exactly in the Authority Envelope, and changed paths must be declared before acceptance. Command verifiers use an argv array and run without a host shell unless the proposal explicitly selects one.

```json
{
  "goal": "Add the requested behavior and its focused test.",
  "execution": {
    "kind": "codex_git",
    "repository": "/absolute/path/to/principal-checkout",
    "base_revision": "0123456789abcdef0123456789abcdef01234567"
  },
  "criteria": [
    {
      "id": "focused-test",
      "description": "The focused test passes in the integrated repository",
      "required_evidence": "focused_test_output",
      "verifier_type": "deterministic",
      "verification_depth": "standard",
      "verifier_configuration": "contained-command-v1",
      "verification_environment": "openshell-repaired-v0.0.104",
      "verifier": {
        "kind": "command",
        "argv": ["cargo", "test", "--test", "focused_test"]
      }
    }
  ],
  "authority": {
    "repositories": ["/absolute/path/to/principal-checkout"],
    "paths": ["src", "tests/focused_test.rs"],
    "actions": ["codex.git_change"],
    "destinations": [],
    "effects": []
  },
  "resource_ceilings": {
    "max_attempts": 1,
    "max_elapsed_seconds": 900,
    "max_worker_concurrency": 1,
    "max_storage_bytes": 104857600,
    "max_model_spend_cents": 500,
    "max_paid_service_spend_cents": 0
  },
  "known_uncertainties": []
}
```

Tyrion copies the selected commit into an independent bundle without mutating the Principal checkout. It uploads only that bundle, the pinned Codex executable, the bounded prompt, the output schema, and its runner. A hard preflight checks Landlock, the process ceiling, CPU ceiling, credential absence, host path absence, runtime socket absence, and undeclared egress denial.

Codex submits a candidate bundle and structured summary. The Control Plane independently verifies the bundle, linear ancestry, commits, and the union of paths touched by every candidate commit. It then runs each criterion in a fresh MicroVM and records immutable candidate Evidence. Only a passing candidate is eligible to enter the daemon-owned integration repository. A third fresh MicroVM records integrated Evidence; the Result becomes accepted in the same transaction as Verified Completion only when that verification passes.

Inspection exposes the Worker configuration, expiring lease, governing mandate revision, base, candidate commits, changed paths, bundle hashes and sizes, candidate and integrated verification outcomes, known effects, and integrated artifact revision. The Principal checkout is never the integration target.

The normal integration suite uses protocol fakes so it is deterministic in CI. It is not boundary attestation. Boundary attestation requires the repaired MicroVM runtime itself. With the pinned gateway and brokered provider running, execute the opt-in real test:

```sh
TYRION_REAL_CODEX_WORKER_CONFIG=/absolute/path/to/codex-worker.json \
  cargo test --test git_commission \
  real_openshell_microvm_completes_the_contained_git_assignment \
  -- --ignored --exact --nocapture --test-threads=1
```

The real path runs the same launch-time probes used by every Attempt: exact process and CPU ceilings, bounded memory and storage, host checkout, sibling and Control Plane state absence, ambient credential and auth-directory absence, runtime-socket absence, Landlock write denial outside `/sandbox`, and deny-default egress. Each probe also starts and confirms a live descendant canary; successful sandbox deletion terminates the canary with the rest of its MicroVM. The test verifies that the Principal and sibling checkouts remain unchanged after artifact transfer and integration. On 2026-08-17, this test passed against the hashes in the example configuration in 48.48 seconds with one Attempt VM, one candidate-verification VM, and one integrated-verification VM.

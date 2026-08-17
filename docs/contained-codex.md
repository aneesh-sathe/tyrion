# Contained Codex Git assignments

Tyrion supports one production Git assignment profile: Codex CLI `0.147.0` inside the repaired OpenShell `0.0.104` MicroVM boundary. The profile pins OpenShell source revision `dd2b4e3bc0688bdd59f90030f7c1d52511d6e354`, the base image digest, the hard Landlock policy, the repaired kernel configuration, every runtime artifact, and the Linux Codex binary. Startup rejects missing or hash-mismatched artifacts. After transfer, the MicroVM itself verifies the guest-only Codex binary version before executing it.

The gateway must use the VM driver with mTLS, 2 vCPUs, 2 GiB memory, a 4 GiB overlay, and the repaired guest's 256-process child cgroup. The checked policy at [`runtime/openshell/hard-landlock-policy.yaml`](../runtime/openshell/hard-landlock-policy.yaml) denies network access unless an explicitly selected OpenShell provider adds a named endpoint policy.

## Configure the Worker

Copy [`runtime/openshell/codex-worker.example.json`](../runtime/openshell/codex-worker.example.json), replace every absolute path, and record the actual SHA-256 of the Linux aarch64 Codex binary. The binary must report `codex-cli 0.147.0`.

`openshell_provider` names an existing OpenShell Codex provider. The provider must expose only `openshell:resolve:env:*` placeholders for the four Codex OAuth fields. Tyrion rejects raw credential values before launch, writes only placeholders to the disposable Codex home, and starts Codex with a cleared environment. No raw model or repository credential enters the Attempt.

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

The normal integration suite uses protocol fakes so it is deterministic in CI. Boundary attestation requires the repaired MicroVM runtime itself. With the pinned gateway and brokered provider running, execute the opt-in real test:

```sh
TYRION_REAL_CODEX_WORKER_CONFIG=/absolute/path/to/codex-worker.json \
  cargo test --test git_commission \
  real_openshell_microvm_completes_the_contained_git_assignment \
  -- --ignored --nocapture
```

The real path runs the same launch-time probes used by every Attempt: exact process and CPU ceilings, bounded memory and storage, host checkout, sibling and Control Plane state absence, ambient credential and auth-directory absence, runtime-socket absence, Landlock write denial outside `/sandbox`, and deny-default egress. Each probe also starts and confirms a live descendant canary; successful sandbox deletion terminates the canary with the rest of its MicroVM. The test verifies that the Principal and sibling checkouts remain unchanged after artifact transfer and integration.

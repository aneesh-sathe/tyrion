# Contained Codex Git assignments

Tyrion supports one production Git assignment profile: Codex CLI `0.147.0`
inside a hardened Docker container. One disposable container holds each
Attempt, each verification run, and each comparison. The boundary is
qualified in
[`prototypes/docker-containment-qualification.md`](prototypes/docker-containment-qualification.md).

On macOS the host protection is the hypervisor: Docker Desktop, Colima, and
Lima all run containers inside a Linux VM under Apple's
Virtualization.framework, so a Worker never shares a kernel with macOS. Docker
maintains that VM, which is why Tyrion no longer builds or ships one. Sibling
Attempts are isolated from each other at namespace strength, not VM strength;
that limit is stated deliberately.

## The boundary

Every sandbox is created with the whole profile, and the daemon sets each
ceiling from outside the container:

| Control | Flag | Effect |
| --- | --- | --- |
| Processes | `--pids-limit 256` | `fork` fails at 256. `/sys/fs/cgroup` is read-only, so guest root cannot raise it. |
| Memory and files | `--memory 6144m --memory-swap 6144m` | One hard ceiling over process memory and the writable tmpfs together. |
| Writable storage | `--mount type=tmpfs,destination=/sandbox,tmpfs-size=4GiB` | The only writable mount. |
| Root filesystem | `--read-only` | Nothing outside `/sandbox` can be modified. |
| CPU | `--cpus 2 --cpuset-cpus 0-1` | A two-core quota that the guest also observes. |
| Privilege | `--cap-drop ALL --security-opt no-new-privileges --user 65534:65534` | No capabilities, no privilege escalation, not root. |
| Syscalls | `--security-opt seccomp=builtin` | Required: Docker Desktop leaves seccomp **unconfined** by default. |
| Network | `--network none`, or a per-Attempt `--internal` bridge | No route off the bridge except through a brokered relay. |
| Host filesystem | no bind mount of any kind | `docker cp` is never used; it silently writes underneath a tmpfs mount instead of into it. |

Before any Worker code runs, a preflight proves all of this from inside the
container: it reads each ceiling from the container's own cgroup, confirms
`CapEff=0`, `NoNewPrivs=1`, a non-zero `Seccomp` mode and a non-root uid,
fails if guest root can raise `pids.max` or write `/etc`, asserts the absence
of the Principal checkout, the daemon state directory, the container runtime
socket, every authentication directory, and every ambient credential variable,
rejects any mount that is not container-owned, confirms undeclared egress is
denied, and starts a descendant canary. A failed preflight is a Security
Invariant violation and the Attempt never launches.

## Provision the runtime

Build the Worker image, then pin it. See
[`runtime/docker/README.md`](../runtime/docker/README.md).

```sh
docker build -t registry.example/tyrion-worker:2026-09-21 runtime/docker
docker image inspect registry.example/tyrion-worker:2026-09-21 \
  --format '{{index .RepoDigests 0}}{{"\n"}}{{.Id}}'
```

Tyrion never pulls. It fails at startup if the pinned image is not already
present locally, and fails again at sandbox creation if `docker inspect`
reports that the container launched anything other than the pinned image ID.
It also pins the Docker CLI by SHA-256 and version, and takes an explicit
`docker_host` rather than resolving an ambient Docker context.

Copy [`runtime/docker/codex-worker.example.json`](../runtime/docker/codex-worker.example.json),
fill in the absolute paths and digests, and record the actual SHA-256 of the
Linux aarch64 Codex binary. It must report `codex-cli 0.147.0`, and Tyrion
probes that version only after uploading it into the container, because it is
a guest-only Linux binary.

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config /absolute/path/to/codex-worker.json
```

## Egress and credentials

Omit `egress` and every sandbox runs with `--network none`.

With `egress`, each Attempt gets its own `--internal` bridge, which Docker
gives no route off itself, plus one relay container per authorized
destination. The relay forwards TCP to exactly one `host:port` and never
terminates TLS, and the Worker reaches it through `--add-host`. The
certificate presented is the real destination's, no CA is injected, and no
other host or address is reachable.

`worker_credentials` names environment variables `tyriond` was started with
that may be forwarded into a Worker execution. It is empty by default:
availability on the host is never permission to use it. Tyrion passes
`docker exec --env NAME`, so Docker reads the value from the daemon process
and it never appears in a command line, in the container's persistent
environment, or in Tyrion's durable state. Because the preflight runs before
any credential is delivered, its assertion that no provider variable exists
stays exactly true.

This is a documented reduction from the previous OpenShell provider, which
kept the credential outside the Attempt entirely and substituted it in the
proxy. Destination pinning still prevents sending it anywhere else, but a
credential inside the Worker is a credential the Worker can use. Provider
access conveys spend and disclosure authority regardless of containment, so
Tyrion's effect gates and the Commission's spend ceilings remain the controls
for that.

## Propose a Git Commission

The immutable base must be a full Git object ID. The repository path must
appear exactly in the Authority Envelope, and changed paths must be declared
before acceptance. Command verifiers use an argv array and run without a host
shell unless the proposal explicitly selects one.

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
      "verification_environment": "docker-hardened-v1",
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

Tyrion copies the selected commit into an independent bundle without mutating
the Principal checkout. It streams only that bundle, the pinned Codex
executable, the bounded prompt, the output schema, and its runner into the
container over `docker exec`.

Codex submits a candidate bundle and structured summary. The Control Plane
independently verifies the bundle, linear ancestry, commits, and the union of
paths touched by every candidate commit. It then runs each criterion in a
fresh container with no network and records immutable candidate Evidence. Only
a passing candidate is eligible to enter the daemon-owned integration
repository. A third fresh container records integrated Evidence; the Result
becomes accepted in the same transaction as Verified Completion only when that
verification passes.

Cleanup never guesses names: every container and network carries a
`tyrion.attempt` label, and removal is confirmed by `docker inspect` failing
rather than by `docker exec`, which would restart a stopped container. Each
container is also started with a command that exits when the Worker Lease
does, so its lifetime is bounded even if Tyrion itself is lost.

## Boundary attestation

The normal integration suite uses protocol fakes so it is deterministic in CI.
It is not boundary attestation. With a provisioned image and a real Docker
daemon, run the opt-in test:

```sh
TYRION_REAL_CODEX_WORKER_CONFIG=/absolute/path/to/codex-worker.json \
  cargo test --test git_commission \
  real_docker_boundary_completes_the_contained_git_assignment \
  -- --ignored --exact --nocapture --test-threads=1
```

It runs the same launch-time probes every Attempt uses and verifies that the
Principal and sibling checkouts are unchanged after transfer and integration.

# Docker Worker runtime

`tyriond --codex-worker-config <path>` takes the JSON in
[`codex-worker.example.json`](codex-worker.example.json). Unknown fields are
rejected, so copy the example rather than editing an older OpenShell profile.

## You do not write this file

`tyrion init` generates it. Every value Tyrion checks at startup can be
discovered from your machine, and a wrong digest makes `tyriond` exit before it
binds its socket, which is correct but opaque. So `init`:

- pins the Docker CLI hash and version, and resolves the current Docker
  context's endpoint once into `docker_host`
- builds the Worker image from [`Dockerfile`](Dockerfile), tagged by a digest of
  its build inputs, and pins its image ID
- downloads the Linux Claude Code and Codex builds for the engine's
  architecture and checks each against its publisher's SHA-256 manifest
- streams each binary into a container under the exact Worker profile and reads
  its version there, which is the only place a Linux guest binary can report
  one and doubles as proof it runs under the profile at all
- names `CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY` in
  `worker_credentials` only if you have it exported, and picks up
  `~/.codex/auth.json` when present, then sets the egress each harness needs
- writes the file, and the Worker catalog, to `$TYRION_DATA_DIR/runtime/`
  (default `~/.local/state/tyrion/runtime/`), where `tyrion claude` and
  `tyrion codex` find them
- starts a throwaway daemon on the result and drives one deterministic
  Commission to `verified_complete`

[`codex-worker.example.json`](codex-worker.example.json) shows the shape.
Unknown fields are rejected.

The image carries the adapters' runtime dependencies, `native_skill` and the
Claude Agent SDK, rather than transferring them per Attempt, so the image ID
Tyrion already verifies is their pin. Tyrion never pulls: it fails at startup if
the pinned image is absent, and again at sandbox creation if the container
launched anything else.

## Fields

| Field | Meaning |
| --- | --- |
| `docker_sha256` | SHA-256 of the Docker CLI this host will run. A silent Docker Desktop upgrade fails closed. |
| `docker_version` | Exact `docker --version` output. |
| `docker_host` | Explicit daemon address. Tyrion never resolves an ambient Docker context. |
| `egress` | Omit for no network at all. Otherwise exactly the destinations a Worker may reach, each behind its own destination-pinned relay on a per-Attempt internal bridge. |
| `worker_credentials` | Names of environment variables `tyriond` was started with that may be forwarded into a Worker execution. Empty by default: availability on the host is not permission to use it. |
| `vcpus`, `memory_mib`, `writable_storage_mib`, `max_processes` | The containment ceilings. Only `2 / 3072 / 2048 / 256` is accepted. |
| `memory_request_mib`, `cpu_request_millis` | What a Worker is expected to use, which admission reserves. Only `320 / 250` is accepted. |

## Host capacity

Every Worker has two sets of numbers. Its **ceilings** (2 vCPUs, 3072 MiB,
2048 MiB of files, 256 processes) are enforced by the container runtime and
contain a runaway. Its **requests** (0.25 CPU, 320 MiB) are what it is expected
to use, and are what admission reserves. They come from measurement: ten real
Codex Workers running at once each peaked at 255-293 MiB and 0.1 cores. See
[`docs/worker-capacity.md`](../../docs/worker-capacity.md).

At startup the daemon asks the container runtime how many CPUs and how much
memory it has (on macOS that is the Docker VM, not the Mac), keeps 1024 MiB back
for the engine and the egress relays, and admits a Worker only while the sum of
every running Worker's requests fits, across all Commissions. A 12-CPU Docker VM
with 7.7 GiB admits 21; with 16 GiB, CPU binds first at 48. Each running container is
pinned to the least loaded CPUs the runtime really has.

If several Workers exceed their requests at once and the container runtime
runs out of memory, it kills a container, not the host. Tyrion sees a failed
Attempt and recovers it like any other.

`tyrion commission inspect` shows the result as `host_capacity`: the figures,
where they came from, the derived Worker ceiling at the pinned request, and
what is reserved. Work that fits its Commission but not the machine appears in
`frontier_holds` as `host_capacity_unavailable`, with the numbers, and
dispatches when running Workers finish. A request the machine could never meet
blocks its Assignment with the exact requirement instead of waiting forever.

`tyriond --host-cpus N --host-memory-mib M` declares capacity instead. It is
the only way to admit more than the runtime reports. Declaring more CPUs than
exist makes Workers share the real ones; declaring more memory risks OOM kills.

## Smaller or heavier Workers

A Worker Configuration in the catalog may declare its own profile. A planning
Worker needs far less than a build Worker, and a harness that has not been
measured should reserve more:

```json
"containment_resources": {
  "vcpus": 1, "memory_mib": 2048, "writable_storage_mib": 1024, "max_processes": 128,
  "memory_request_mib": 384, "cpu_request_millis": 100
}
```

Its ceilings may only shrink the pinned ones: at least 1024 MiB memory, 256 MiB
storage and 64 processes, with 512 MiB of memory above storage. Its requests
may be anything from 256 MiB and 50 millicores up to its own ceilings. The
containment preflight proves the declared ceilings from inside the container,
exactly as it proves the pinned ones.

`memory_mib` bounds process memory and the writable `/sandbox` tmpfs together,
because tmpfs pages are charged to the container memory cgroup.
`writable_storage_mib` is the separate hard sub-ceiling on files.

## Credentials

A forwarded credential is named, never valued, in this file. Tyrion passes
`docker exec --env NAME` so the value is read from the daemon process: it never
appears in a command line, in the container's persistent environment, or in
Tyrion's durable state, and it is scoped to the single execution that needs it.
The containment preflight runs before any credential is delivered and asserts
that no provider variable is present.

The relay forwards TCP without terminating TLS, so a credential stays
end to end encrypted to its destination and cannot be sent anywhere else.
It does not bound spend or disclosure at that destination. Tyrion's effect
gates bound what is sent, and your provider's spend cap bounds cost.

## Reading the work a Commission produced

Accepted work lands in a Git repository the daemon owns, never in your
checkout:

```sh
REPO="$TYRION_DATA_DIR/integrations/$COMMISSION_ID/repository"
git fetch "$REPO" tyrion-integration
git diff HEAD FETCH_HEAD          # review before accepting anything
git merge --ff-only FETCH_HEAD    # the only moment your checkout changes
```

`tyrion commission inspect $COMMISSION_ID` reports the accepted artifact
revision, the changed paths, and the Evidence behind them.

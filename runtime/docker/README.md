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
| `vcpus`, `memory_mib`, `writable_storage_mib`, `max_processes` | The containment ceilings. Only `2 / 6144 / 4096 / 256` is accepted. |

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

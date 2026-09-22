# Docker Worker runtime

`tyriond --codex-worker-config <path>` takes the JSON in
[`codex-worker.example.json`](codex-worker.example.json). Unknown fields are
rejected, so copy the example rather than editing an older OpenShell profile.

## Provision the image

Build from the repository root, so the adapter sources are in context:

```sh
docker build -f runtime/docker/Dockerfile -t registry.example/tyrion-worker:2026-09-22 .
docker push registry.example/tyrion-worker:2026-09-22
docker image inspect registry.example/tyrion-worker:2026-09-22 \
  --format '{{index .RepoDigests 0}}{{"\n"}}{{.Id}}'
```

The image carries the adapters' runtime dependencies -- `native_skill` and the
Claude Agent SDK -- rather than transferring them per Attempt. They are then
covered by the image digest Tyrion already verifies at launch, instead of
needing a second pinned artifact kept in step with the first. Confirm a build
can satisfy a real adapter:

```sh
docker run --rm -e PYTHONPATH=/opt/tyrion <image> \
  python3 -c 'import native_skill, claude_agent_sdk; print("ok")'
```

Put the digest reference in `worker_image` and the image ID in
`worker_image_id`. Tyrion never pulls: it fails at startup if the pinned image
is not already present, and it fails again at sandbox creation if the
container launched anything else.

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
It does not bound spend or disclosure at that destination; Tyrion's effect
gates and the Commission's spend ceilings remain the controls for that.

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

# Working on Tyrion

Notes for contributors and the coding agents they work with. The README says
what Tyrion is; `docs/how-it-works.md` explains a job end to end; this file is
about changing the code without breaking what it promises.

## Before you commit

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

All four must pass. A flaky test is a bug to fix, not to retry. Use commit
messages of the form `feat: ...`, `fix: ...`, `docs: ...`, `test: ...` or
`refactor: ...`.

Focused suites, by area:

| Suite | Covers |
| --- | --- |
| `git_commission` | contained Git jobs: lifecycle, leases, containment failures, path authority, transfer, integration, planning, capacity, uninstall |
| `cross_harness_routing` | routing gates and ranking, fallback, attention, Worker handles, steering and interruption, per-harness egress |
| `production_adapters` | the real Python adapters against fake harness processes |
| `commission_effects` | operation classification, approval gates, amendments, cancellation |
| `credentialed_effects` | brokered credentials, one-shot sandboxes, leak checks, no-replay recovery (macOS) |
| `commission_recovery`, `commission_lifecycle` | recovery, restart, verification and completion |
| `learning` | preferences: promotion, correction, forgetting, boundaries, packets, receipts |

Two opt-in tests exercise the real boundary and real models. Run `tyrion init`
with a Codex login first:

```sh
TYRION_REAL_CODEX_WORKER_CONFIG=~/.local/state/tyrion/runtime/worker-runtime.json \
  cargo test --test git_commission real_docker -- --ignored
TYRION_REAL_WORKER_RUNTIME=~/.local/state/tyrion/runtime/worker-runtime.json \
TYRION_REAL_WORKER_CATALOG=~/.local/state/tyrion/runtime/worker-catalog.json \
  cargo test --test git_commission real_opencode -- --ignored
```

On macOS, accept the Xcode license (`sudo xcodebuild -license accept`) first:
until then `git` exits 69 under the daemon's cleared environment.

## Code map

| Path | Responsibility |
| --- | --- |
| `src/bin/tyrion.rs`, `src/bin/tyriond.rs` | CLI and daemon entry points |
| `src/daemon.rs` | socket server, data-directory lock, startup recovery |
| `src/store.rs` | every lifecycle transaction |
| `src/store/projection.rs` | read projections (inspection, export) |
| `src/store/schema.rs` | schema (v18), migrations, integrity checks |
| `src/store/frontier.rs`, `src/store/planning.rs` | dispatch frontier and admission; the planning Worker |
| `src/worker/contained_codex.rs` | Docker sandboxes, networks, transfer, verification |
| `src/worker/routing.rs` | Worker catalog and routing |
| `src/worker/adapter_contract.rs`, `src/worker/structured_process.rs` | the shared adapter trace contract |
| `adapters/*.py` | Codex, Claude, OpenCode and Pi adapters; `native_skill.py` is shared and lives in the Worker image |
| `src/entry_mcp.rs`, `src/native_entry_launcher.rs`, `src/digest.rs` | the Claude Code and Codex Entry Session bridge and its status digest |
| `src/credential.rs` | brokered credentials and effect sandboxes |
| `src/init.rs`, `src/uninstall.rs` | machine setup and removal |
| `runtime/docker/` | the Worker image and runtime file reference |

Keep lifecycle writes in `store.rs`, reads in `store/projection.rs`, and schema
rules in `store/schema.rs`.

## Vocabulary

Code, protocol and CLI output use these terms precisely: Principal (the user),
Commission (one delegated job), Commission Proposal, Acceptance Criterion,
Authority Envelope, Assignment (one planned piece), Attempt (one Worker's run
of an Assignment), Worker, Result, Evidence, Control Plane, Blocker and
Verified Completion. User-facing prose says "job" and "agent" instead.

## Invariants

These are the promises. A change that weakens one needs its own discussion.

**The seam and the store**

- The public seam is the `tyrion` CLI over the versioned Unix-socket protocol
  to `tyriond`. End-to-end tests observe only that seam, with real SQLite state
  and real daemon restarts.
- `tyriond` is the only writer. It holds an exclusive lock per data directory,
  keeps the directory and socket user-only, and runs SQLite in WAL mode.
- Read-only projections read through one deferred transaction, so every query
  sees the same snapshot.
- Mutating requests carry idempotency keys. An identical replay returns the
  stored response; reusing a key for a different request is refused. A used
  attachment handshake is the exception: it never replays its credential.
- Proposal, criterion, authority and ceiling JSON refuse unknown fields. The
  proposal field is `commission_constraints`.
- Schema changes back the database up once, migrate, verify integrity, then
  delete the backup. Bumping `PROTOCOL_VERSION` also advances the
  incompatible-handshake test fixture.

**Authority**

- Creating a proposal grants nothing. Acceptance needs the exact expected
  revision and an idempotency key.
- Harness capability is a ceiling, never a grant. Effective authority is the
  intersection of harness capability, the accepted Authority Envelope, and the
  Attempt's current, expiring Worker Lease.
- Every operation is classified as silent and journaled, a non-blocking
  notification, an Approval Gate, or prohibited. An approved operation must
  match its canonical digest and current revisions at execution time.
- Only the Principal approves, with a credential the daemon hands out once
  over an inherited descriptor numbered 3 or higher
  (`--principal-control-bootstrap-fd`). It never persists, logs or reaches an
  Attachment or Worker.
- A `codex_git` Commission may carry exactly one effect kind, a local
  `filesystem.write` the daemon performs itself, and only to a directory
  outside the Principal's checkout.

**Verification and completion**

- Evidence is immutable and bound to criterion, mandate revision, candidate
  Result, verifier and artifact revision. The Control Plane recomputes artifact
  hashes itself.
- A Result stays a candidate until fresh integrated verification passes.
  Verified Completion, the accepted Result, passed criteria, the briefing and
  the terminal event commit in one transaction.
- Integrated verification checks only criteria whose work is in the assembled
  artifact (any Assignment with an `integrated_artifact_revision`).
- A verifier that cannot run blocks once as `verifier_unrunnable`; it never
  reruns the Worker.
- Accepting a Result, planned or not, records its Profile Claim outcomes, so
  every accepted Attempt gets a Learning Receipt.

**Recovery**

- Acceptance and readiness commit before dispatch. A disconnected Entry
  Session never revokes accepted work.
- Each ready Assignment dispatches on its own connection and thread.
- Restart never reattaches a Worker in memory. It records what it could not
  prove, expires the Lease, deletes the sandboxes, restores the integration
  repository, and only then retries. Pending sandbox deletion blocks dispatch.
- Recovery retries a transient failure once on the same configuration,
  reroutes an unavailable one immediately, and revises the plan after a second
  equivalent failure. An acknowledged integrated Result is never re-executed.
- An effect whose outcome is uncertain is reconciled read-only or becomes a
  Blocker. It is never retried blindly.

**Containment (`docker-hardened-v1`)**

- One disposable container per Attempt, verification run and comparison:
  `--read-only --pids-limit 256 --memory 3072m --memory-swap 3072m --cpus 2
  --cpuset-cpus <two least-loaded> --cap-drop ALL --security-opt
  no-new-privileges --security-opt seccomp=builtin --user 65534:65534`, one
  sized `/sandbox` tmpfs as the only writable mount, and no bind mount.
- The runtime file accepts only that profile, with requests of 250 millicores
  and 320 MiB. Admission sums requests across every Commission, keeping 1 GiB
  back; the container runtime enforces the ceilings.
- The Principal's checkout is an input, never a workspace. Code moves in and
  out as verified Git bundles. A candidate is rejected if it touches an
  unauthorized path or contains a symlink that escapes the repository.
- A containment preflight proves the profile from inside before any Worker
  code runs. A failed preflight is a Security Invariant failure.
- Siblings are isolated at namespace strength. Never claim virtual-machine
  isolation between Workers.
- Harness binaries are built into the Worker image, which the image ID pins.
  Every sandbox rechecks the pinned version (`CODEX_VERSION`,
  `OPENCODE_VERSION`) before launch.

**Egress and credentials**

- A Worker's network is a per-Attempt `--internal` bridge plus one relay per
  allowed destination. Relays forward TCP to exactly one `host:port` without
  terminating TLS. Destinations are scoped per harness.
- Attempt networks get explicit `/28` subnets from `10.213.0.0/16`. Docker's
  default pool holds only about 30 networks.
- Worker credentials are named, never valued, in configuration, and delivered
  after preflight: environment variables by `docker exec --env NAME`, the Codex
  login as token fields streamed from memory. Nothing secret reaches a command
  line, a file on the host, or the database.

**Learning**

- Learned preferences are advisory. Commission constraints, criteria,
  authority and ceilings always win, and memory never changes routing,
  approvals, credentials or ceilings.
- Every adapter renders the Worker Context Packet into the model's prompt:
  binding constraints first, then advisory preferences
  (`native_skill.context_packet_lines`, mirrored by `context_packet_text` in
  Rust).
- A reusable preference is one atomic sentence, with no "and", commas or
  semicolons.

## Pitfalls that cost real debugging time

**Docker**

- `--security-opt seccomp=builtin` is mandatory: Docker Desktop leaves seccomp
  unconfined by default.
- Never use `docker cp` into a sandbox: on Docker Desktop it exits 0 while
  writing underneath the tmpfs. Stream with `docker exec -i ... cat`.
- `docker exec` forwards no stdin without `--interactive`.
- Never probe liveness with `docker exec`; it restarts a stopped container.
  Confirm removal by `docker inspect` failing.
- Docker mounts tmpfs `noexec` by default; `/sandbox` needs `exec`.
- The memory cgroup charges tmpfs pages, so the memory ceiling must exceed
  writable storage. `--storage-opt size=` does not work on `overlayfs`.
- Label every container and network `tyrion.attempt=<id>` and clean up by
  label, never by reconstructing names.
- Size the CPU allocator from Docker's real `NCPU`; asking for a CPU that does
  not exist fails container creation.
- Worker clones exclude runtime byproducts (`__pycache__/`, `node_modules/`,
  and so on). The list lives in both `native_skill.py` and
  `contained_codex.rs`, and a test keeps them identical.
- A locally built image has an image ID but no registry digest. Accept either,
  never a tag.

**Harnesses**

- Codex needs its companion `codex-code-mode-host` binary to edit files, and
  reads its login from `auth.json`, not the environment. Its subscription
  egress is `chatgpt.com:443` plus `auth.openai.com:443`.
- Apply Codex `reasoning_effort` to both the thread and the turn, and validate
  the thread's effective effort.
- OpenAI structured output rejects an array schema without `items`; Claude
  accepts it. A second harness is what audits the first.
- The Claude Agent SDK waits for an input iterable to finish, so send each
  turn with its own `client.query`. Expect its `StructuredOutput` tool in the
  inventory without granting it.
- In Claude Code print mode the Entry bridge needs `--allowedTools
  mcp__tyrion`; Codex needs `default_tools_approval_mode="approve"`.
- OpenCode runs as `opencode serve`, driven by `prompt_async` and its SSE
  event stream. `opencode run --attach` buffers output and ignores abort. It
  needs `TMPDIR` under `/sandbox`, and it delivers no native Skills, so it never
  claims the `skills` capability.
- Pi RPC supports one native Skill per Assignment and must clear its queue
  before abort.

**Daemon and tests**

- Wait for the daemon with `daemon_is_ready`, not for the socket file: the
  socket binds before startup finishes.
- Startup dispatch is asynchronous; poll the projection for the state you
  expect. A structured adapter reports its session ID before its start event,
  so wait for both.
- Synchronize control tests on explicit fixture signals, never on sleeps.
- `--watchdog-stall-milliseconds` defaults to ten minutes on purpose: real
  models are silent for minutes.

## Fakes and evidence

The default suite runs against fakes of Docker and each harness
(`tests/fixtures/`). A fake that agrees with the adapter instead of behaving
like the real program hides real defects, and the first run against each real
harness found several. When a fake and reality disagree, change the fake.
When a new test passes first time, break the code once to prove the test can
fail.

## Docs

- User-facing docs use plain words ("agent", "job") and keep internal
  vocabulary for the reference pages.
- Every number in the README must trace to an entry in `docs/results.md`.
  Ceilings computed from the admission rule are labelled as ceilings, never
  presented as measurements.
- Never commit an exported Commission record, a timeline, or any absolute home
  path: they carry machine-specific details.
- Keep project-internal notes in `CLAUDE.local.md`, which git ignores.

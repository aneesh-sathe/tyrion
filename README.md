# Tyrion

Tyrion is a local control plane for coding agents. You give it a Goal, define what proof counts, approve a bounded mandate, and let it coordinate the work. Tyrion keeps the durable record. Agent sessions and models can come and go.

A Worker may produce a candidate Result, but it cannot declare its own work accepted. Tyrion checks the Result against the accepted criteria, integrates verified Git changes into daemon-owned state, and reports either Verified Completion or a concrete Blocker.

This repository is a dogfood MVP, not a packaged end-user release. The deterministic path runs on a Unix host with no model account. The contained production Worker path targets Apple Silicon macOS and needs Docker plus a pinned Worker image.

Both Tier 1 Worker harnesses run real models under that boundary today. On 2026-09-24 a single Commission ran Codex `gpt-5.6-sol` and Claude Haiku concurrently on disjoint Assignments and produced one verified integrated artifact, 20.1 seconds faster than running them in sequence. The exported records are checked in under [`docs/dogfood-records/`](docs/dogfood-records/).

## Why Tyrion exists

Coding agents are good at doing work inside one session. Long jobs become harder when the session dies, several agents edit in parallel, a model reports success too early, or an external action needs approval.

Tyrion owns those parts of the job:

- Durable Commission state in SQLite
- Explicit Goals, Acceptance Criteria, authority, and resource ceilings
- Ordered events and reconnectable Entry Sessions
- Routing across complete Worker Configurations
- Expiring Worker Leases and externally enforced containment
- Candidate and integrated verification before acceptance
- Exact Approval Gates for consequential effects
- Recovery that preserves failed Attempts, Evidence, and useful completed work
- Scoped, inspectable software-building preferences

Tyrion does not recreate a model loop. Codex, Claude, and Pi keep their native tools and Skills. The daemon gives each selected Worker one bounded Assignment and judges the returned Result independently.

## The vocabulary

- A Principal is the person who accepts the mandate and approves consequential changes.
- A Commission is the durable unit of work. It contains the Goal, criteria, authority, ceilings, plan, history, and outcome.
- An Entry Session is the Agent Harness session used to inspect or control a Commission.
- An Assignment is one planned piece of a Commission.
- An Attempt is one Worker's bounded execution of an Assignment.
- A Result is a candidate output. It remains unaccepted until verification passes.
- Evidence records why a criterion passed, failed, or remains uncertain.
- Verified Completion means every current criterion passed and every required gate closed.

These names appear in the CLI output and protocol. They are worth learning because Tyrion uses them precisely.

## Start a native Entry Session

For the normal interactive path, build Tyrion and launch the Agent Harness already installed on `PATH`:

```sh
cargo build
target/debug/tyrion claude
# or
target/debug/tyrion codex
```

Run the command anywhere inside the Git repository you want to work on. Tyrion finds the repository root, creates a private local state directory, starts `tyriond` when needed, injects a session-only Tyrion MCP server and instructions, and then hands the terminal to the harness's native TUI. It does not edit Claude, Codex, or repository configuration files. If this launch started the daemon, exiting the TUI stops that daemon but retains the durable SQLite state.

The MVP permits one auto-managed native TUI at a time so one session cannot accidentally terminate another session's daemon. One TUI can run many sequential Commissions. Multiple simultaneous Entry Sessions require an explicitly managed daemon and `--socket`.

Talk to Claude or Codex normally. For each substantial task, the harness constructs and accepts one bounded Commission through Tyrion. The same TUI session can host multiple sequential Commissions. Exact retries reuse the first Commission, overlapping tasks are rejected, and abandoned blocked work can be cancelled before starting again. The built-in path does not ask the user for proposal JSON, socket paths, launch tokens, or attachment credentials.

Pass native harness arguments after `--`:

```sh
target/debug/tyrion claude -- --model opus
target/debug/tyrion codex -- --model gpt-5.5
```

The first MVP path is deliberately small. It permits one Assignment and Attempt, one Worker, at most 15 minutes, 100 MiB of storage, and $1 of model spend. It rejects paid services, external effects, non-deterministic verification, and actions other than `deterministic.echo` or `codex.git_change`.

The native TUI is an Entry Session, not an uncontained Worker. The deterministic Worker works without setup. A real `codex_git` Commission still needs an eligible contained Worker runtime as described below. Until Tyrion can provision that runtime itself, the one-command launcher proves the user-facing control path but does not by itself satisfy the production dogfood claim.

## Run the deterministic walkthrough

This is the right first run. It exercises the real CLI, Unix socket, daemon, SQLite state, attachment handshake, ordered events, dispatch, Evidence, and Verified Completion. The built-in deterministic Worker only echoes the accepted Goal, so it needs no external runtime or credential.

### Requirements

- A current stable Rust toolchain
- A Unix-like host
- `jq` for the copyable shell commands below

Build both binaries:

```sh
cargo build
```

In the first terminal, start the daemon with a fresh data directory:

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-demo \
  --socket .scratch/tyrion-demo/tyrion.sock
```

The daemon keeps running in the foreground. It creates a private data directory, a permission-restricted socket, and `.scratch/tyrion-demo/state.sqlite3`.

In a second terminal, create `proposal.json`:

```json
{
  "goal": "return a deterministic greeting",
  "criteria": [
    {
      "id": "greeting",
      "description": "The Result matches the accepted greeting",
      "required_evidence": "exact_output",
      "verifier_type": "deterministic",
      "verification_depth": "standard",
      "verifier": {
        "kind": "exact_match",
        "expected": "return a deterministic greeting"
      }
    }
  ],
  "authority": {
    "repositories": [],
    "paths": [],
    "actions": ["deterministic.echo"],
    "destinations": [],
    "effects": []
  },
  "resource_ceilings": {
    "max_attempts": 1,
    "max_elapsed_seconds": 30,
    "max_worker_concurrency": 1,
    "max_storage_bytes": 1048576,
    "max_model_spend_cents": 0,
    "max_paid_service_spend_cents": 0
  },
  "known_uncertainties": []
}
```

Set two shortcuts:

```sh
TYRION=target/debug/tyrion
TYRION_SOCKET=.scratch/tyrion-demo/tyrion.sock
```

Issue a single-use launch token for the Entry Session:

```sh
LAUNCH_TOKEN=$(
  "$TYRION" --socket "$TYRION_SOCKET" attachment issue-token \
    --harness codex \
    --adapter-identity codex-mcp-entry \
    --adapter-version 1.0.0 \
    --idempotency-key demo-issue-token |
  jq -r .launch_token
)
```

Connect a Full Entry Session. Tyrion binds the launch token to the harness identity, adapter version, protocol version, native session ID, and advertised capabilities.

```sh
ATTACHMENT_SESSION_TOKEN=$(
  "$TYRION" --socket "$TYRION_SOCKET" attachment connect \
    --token "$LAUNCH_TOKEN" \
    --harness codex \
    --adapter-identity codex-mcp-entry \
    --adapter-version 1.0.0 \
    --native-session-id readme-demo \
    --capability proposal_creation \
    --capability commission_acceptance \
    --capability commission_inspection \
    --capability event_replay \
    --capability control_takeover \
    --capability material_notifications \
    --capability persistent_mode_display \
    --capability worker_steering \
    --capability worker_interruption \
    --idempotency-key demo-connect |
  jq -r .attachment_session_token
)
```

The session token is a credential. Keep it out of repositories and logs. A public Attachment ID identifies a session in projections but grants no control.

Create the Commission:

```sh
CREATED=$(
  "$TYRION" --socket "$TYRION_SOCKET" \
    --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
    proposal create \
    --file proposal.json \
    --idempotency-key demo-proposal
)

COMMISSION_ID=$(printf '%s' "$CREATED" | jq -r .commission.id)
printf '%s\n' "$CREATED" | jq
```

Proposal creation grants no execution authority. Inspect the Commission, then accept revision `0`:

```sh
"$TYRION" --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  commission inspect "$COMMISSION_ID"

"$TYRION" --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  commission accept "$COMMISSION_ID" \
  --expected-revision 0 \
  --idempotency-key demo-accept
```

Acceptance commits before dispatch. Inspect again until `.commission.status` is `verified_complete`:

```sh
"$TYRION" --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  commission inspect "$COMMISSION_ID" |
  jq '{commission: (.commission | {status, revision}), criteria, results, evidence, briefing}'
```

The final projection contains the accepted Result, criterion-linked Evidence, and a completion briefing. Stop the daemon with `Ctrl-C` in the first terminal.

Use a new data directory or new idempotency keys when repeating the walkthrough. Tyrion returns the stored response for an identical replay and rejects the same key with different input.

## How a real Commission runs

1. An Entry Session consumes a short-lived launch token and negotiates its capabilities. Tyrion derives Full, Limited, or Observer mode from the accepted manifest.
2. The Entry Session submits a Commission Proposal. The proposal names the Goal, Acceptance Criteria, Authority Envelope, resource ceilings, and known uncertainties.
3. The Principal reviews and accepts an exact revision. No Worker can start before this point.
4. The daemon creates Assignments, reserves their complete resource budgets, and routes each one to an eligible Worker Configuration.
5. A Worker receives one revision-bound launch message and an expiring Lease. It returns a candidate Result rather than a completion claim.
6. Tyrion validates the candidate, runs the required checks, and integrates accepted Git work into daemon-owned state.
7. Tyrion verifies the integrated artifact again. It commits Verified Completion only when every current criterion passes.

If a check fails, Tyrion keeps the Evidence and chooses a concrete recovery action. It may retry a transient failure once, route to a better fit, create a reconciliation Assignment, or stop at an actionable Blocker. A daemon restart does not erase this history.

## Authority and effects

Harness capability is a technical limit, not permission. Effective authority is the intersection of three things:

- What the Entry Session or Worker can do
- What the accepted Commission allows
- What the current Assignment and Worker Lease grant

Consequential operations use exact, single-use Approval Gates. The approval binds the current revisions, target identity, parameters, consequences, limits, and operation digest. A changed request needs a new approval. Tyrion never treats ambient credentials or installed tools as authority.

Credentialed effects use the macOS Keychain broker and, when necessary, a fresh one-shot Effect Sandbox. Secret values stay outside SQLite, Worker environments, Entry Sessions, command arguments, Evidence, and durable receipts. See [Credentialed effects](docs/credentialed-effects.md).

## Entry Sessions and Workers

An Entry Session may observe many Commissions, but each Commission has one Active Attachment. Other Attachments remain observers until an explicit revision-checked takeover. Disconnecting every Entry Session does not stop accepted daemon work.

Missing capabilities include the affected protocol operations, the practical effect, a concrete alternative, and a supported harness that can restore Full control. Capability loss removes affected controls and records an ordered `attachment_capabilities_changed` event.

Tyrion routes the whole Worker Configuration, not a model name by itself. A configuration includes the Agent Harness, adapter version, model settings, tools, native Skills, context strategy, resource limits, authority compatibility, containment profile, availability, and measured outcomes. The Entry Session's harness does not receive a routing preference.

The repository contains reference structured adapters for Codex app-server, Claude Agent SDK, and qualified Pi RPC. Production eligibility requires the complete shared lifecycle, usage, interruption, Result, Skill, restart, and containment contract. Terminal text scraping does not qualify a Worker.

## Run with production Workers

The deterministic walkthrough does not prove the containment boundary. Real Git work needs Docker, a pinned Worker image, and the pinned harness binaries.

Do not begin by guessing values in the runtime JSON. Startup verifies paths, versions, hashes, the Docker CLI identity, the Worker image identity, and the resource ceilings, and it refuses to start rather than run something unverified. Follow the setup document for the role you need:

- [Contained Codex Git assignments](docs/contained-codex.md)
- [Cross-harness Worker routing and control](docs/cross-harness-workers.md)
- [Pi Entry and Worker adapters](docs/pi-adapter.md)
- [Credentialed effects](docs/credentialed-effects.md)

The daemon accepts these optional runtime files:

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config /absolute/path/to/codex-worker.json \
  --worker-catalog /absolute/path/to/worker-catalog.json \
  --credential-runtime /absolute/path/to/credential-runtime.json
```

Generate the runtime file rather than writing one:

```sh
runtime/docker/generate-config.sh --image <your-image> --out .scratch/runtime \
  --claude /path/to/claude-linux-arm64
```

It discovers every value Tyrion verifies, and runs each harness binary inside the hardened container to read its version. [`runtime/docker/codex-worker.example.json`](runtime/docker/codex-worker.example.json) documents the shape and [`runtime/docker/README.md`](runtime/docker/README.md) explains each field. It is not ready to run until every path and digest matches your machine.

`--credential-runtime` drives the exceptional one-shot credentialed Effect Sandbox. It uses the same hardened Docker profile as a Worker, on a per-operation internal network whose only route out is a relay pinned to the one approved destination.

## Inspect and control work

`commission inspect` returns the accepted mandate, plan revisions, Assignment frontier, Attempts, Worker Handles, routing decisions, reservations, Results, Evidence, recovery history, current controls, and completion briefing.

Export a portable, integrity-checked Commission Record after completion or when preserving an actionable Blocker:

```sh
target/debug/tyrion --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  commission export-record COMMISSION_ID > commission-record.json
```

The `sha256:` checksum covers the complete `record` value, while `exported_at` remains export metadata. The record includes the accepted mandate, routing and Worker configurations, Attempts, Results, Integration, Evidence, effects and Approval Gates, recovery, learning receipts, terminal events, and a metric-separated final run report. Its summary states that exported containment Evidence is not independent runtime attestation and calls out fixture-backed Workers explicitly. `dogfood_readiness.status` is fail closed: fixture evidence, an incomplete Commission, a Security Invariant failure, or an unreconciled effect produces `blocked`; an otherwise clean export remains `unassessed` and never automatically claims readiness.

The Active Attachment can steer or interrupt a live structured Worker when both the Entry Session and selected Worker Configuration support that command:

```sh
"$TYRION" --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  worker steer "$COMMISSION_ID" Arya \
  --clarification "Keep the accepted API contract unchanged." \
  --expected-revision CURRENT_REVISION \
  --idempotency-key steer-arya

"$TYRION" --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  worker interrupt "$COMMISSION_ID" Arya \
  --reason "Stop this Attempt." \
  --planned-uncertainty "The interruption will exercise durable recovery." \
  --expected-revision CURRENT_REVISION \
  --idempotency-key interrupt-arya
```

Steering may clarify an Assignment. It cannot change the Goal, criteria, authority, or ceilings. Interruption revokes the live Lease and preserves the Attempt in history. `--planned-uncertainty` must exactly match a known uncertainty in the accepted mandate, which prevents an ad hoc intervention from being relabeled after the fact. The final briefing reports those mandate-bound controls separately from unplanned Principal interventions.

Use the built-in help for the full command tree:

```sh
target/debug/tyrion --help
target/debug/tyrion commission --help
target/debug/tyrion worker --help
target/debug/tyrion principal --help
```

## Repository map

- `src/bin/tyriond.rs` starts the local daemon.
- `src/bin/tyrion.rs` implements the command-line client and Pi launcher.
- `src/store.rs` owns lifecycle transactions.
- `src/store/projection.rs` builds public Commission views.
- `src/store/schema.rs` owns schema and migration invariants.
- `src/worker/` contains routing, adapter contracts, containment, and execution.
- `adapters/` contains the reference structured Worker adapters and Pi Entry extension.
- `runtime/docker/` contains the Worker image, its runtime configuration example, and setup notes.
- `docs/dogfood-records/` holds checksummed exported Commission records from real runs.
- `tests/` exercises the public CLI and socket protocol with real SQLite state and daemon restarts.

## Verify the repository

Run the same checks required before a commit:

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The default suite uses deterministic protocol fakes for external Agent Harnesses. The ignored real-runtime test is a separate boundary attestation and needs the Docker setup described in [Contained Codex Git assignments](docs/contained-codex.md).

Fakes are a convenience, not evidence. Running real harnesses for the first time surfaced ten defects the green suite could not, every one of them because a fake agreed with the adapter instead of behaving like the real harness. Where the two now disagree, the fake was changed.

## Project status

Tyrion is built for one Principal on one local machine. It is not a multi-user service, a general workflow engine, or a claim that an agent can safely act without bounded authority and independent checks.

Proven with real models, with a checked-in record for each:

- A Commission driven end to end from an Entry Session, the same MCP interface Claude Code holds open.
- Codex and Claude running concurrently on disjoint Assignments, producing one verified integrated artifact and measurably beating serial execution.
- Candidate and integrated verification, each in a separate fresh container.
- Interruption, and restart recovery against a Worker container genuinely orphaned by killing the daemon mid-Attempt.

Measured, not assumed:

- The containment boundary, attacked directly rather than described. Every escape attempt blocked; the probe is `.scratch/docker-qual-20260921/escape.sh` and the results are in [the qualification](docs/prototypes/docker-containment-qualification.md).
- A completed Commission leaves the Principal checkout byte-identical, asserted over every path, mode and content digest.

Stated limits, because they matter more than the claims:

- **Tyrion does not bound model spend.** No harness gives it a hard monetary ceiling, so it does not pretend to enforce one. A budget a harness can honour is configured on that Worker; the spend control is the cap you set at the provider.

- Sibling Attempts are isolated at namespace strength, not VM strength.
- Containment does not bound what a provider credential can spend at the far end.
- The exporter reports readiness only as `blocked` or `unassessed`. It never certifies itself ready.

The full product definition and testing decisions live in [issue 1](https://github.com/aneesh-sathe/tyrion/issues/1). Remaining work is tracked in the repository issues.

## License

[MIT](LICENSE)

# Tyrion

Tyrion is a local control plane for coding agents. You give it a Goal, define what proof counts, approve a bounded mandate, and let it coordinate the work. Tyrion keeps the durable record. Agent sessions and models can come and go.

A Worker may produce a candidate Result, but it cannot declare its own work accepted. Tyrion checks the Result against the accepted criteria, integrates verified Git changes into daemon-owned state, and reports either Verified Completion or a concrete Blocker.

This repository is a dogfood MVP, not a packaged end-user release. The deterministic path runs on a Unix host with no model account. The contained production Worker path currently targets Apple Silicon macOS and requires a repaired, pinned OpenShell runtime.

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

The deterministic walkthrough does not prove the MicroVM boundary. Real Git work requires the repaired OpenShell runtime and pinned artifacts.

Do not begin by guessing values in the runtime JSON. Startup verifies paths, versions, hashes, the OpenShell source revision, policy files, the base image, VM limits, and native binaries. Follow the setup document for the role you need:

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

`runtime/openshell/codex-worker.example.json` documents the required shape. It is not ready to run until every path and digest matches the local repaired runtime.

## Inspect and control work

`commission inspect` returns the accepted mandate, plan revisions, Assignment frontier, Attempts, Worker Handles, routing decisions, reservations, Results, Evidence, recovery history, current controls, and completion briefing.

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
  --expected-revision CURRENT_REVISION \
  --idempotency-key interrupt-arya
```

Steering may clarify an Assignment. It cannot change the Goal, criteria, authority, or ceilings. Interruption revokes the live Lease and preserves the Attempt in history.

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
- `runtime/openshell/` contains pinned policies, the repair patch, and runtime examples.
- `tests/` exercises the public CLI and socket protocol with real SQLite state and daemon restarts.

## Verify the repository

Run the same checks required before a commit:

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The default suite uses deterministic protocol fakes for external Agent Harnesses. The ignored real-runtime test is a separate boundary attestation and requires the repaired OpenShell setup described in [Contained Codex Git assignments](docs/contained-codex.md).

## Project status

Tyrion is built for one Principal on one local machine. It is not a multi-user service, a general workflow engine, or a claim that an agent can safely act without bounded authority and independent checks.

The full product definition and testing decisions live in [issue 1](https://github.com/aneesh-sathe/tyrion/issues/1). Current implementation work is tracked in the repository issues.

## License

[MIT](LICENSE)

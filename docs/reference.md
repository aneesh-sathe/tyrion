# Reference

Detail that used to live in the README. Start with the [README](../README.md) if you have not read it.

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

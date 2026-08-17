# Tyrion

Tyrion is a durable local Control Plane for software-building Commissions. The walking skeleton implements the deterministic lifecycle from [issue #2](https://github.com/aneesh-sathe/tyrion/issues/2), the durable Entry Session attachment contract from [issue #3](https://github.com/aneesh-sathe/tyrion/issues/3), and the contained Codex Git path from [issue #4](https://github.com/aneesh-sathe/tyrion/issues/4). A Principal connects an explicit Entry Session, reviews and accepts a proposal through the Active Attachment, and receives Verified Completion only after current integrated Evidence passes.

## What exists

- A versioned JSON protocol over a permission-restricted Unix socket
- A single-owner local daemon with SQLite WAL persistence
- Single-use launch tokens bound to a harness, adapter identity, adapter version, and short expiry
- Granular capability negotiation with visible Full, Limited, or Observer modes
- Multiple durable observers with exactly one explicit Active Attachment per Commission
- Explicit control takeover and cursor-based replay of durable ordered events
- Explicit proposal and acceptance commands with revision preconditions
- Durable acceptance before Worker dispatch, with the ready Assignment returned to the Principal
- Durable idempotency keys for mutating requests
- One replaceable deterministic local Worker configuration
- One precisely pinned Codex Git Worker configuration inside a repaired OpenShell MicroVM
- Expiring Worker Leases with fail-closed sandbox deletion
- Verified Git-bundle transfer without mounting the Principal checkout
- Candidate verification, daemon-owned Integration, and fresh integrated verification
- Criterion-linked deterministic Evidence with independently validated artifact revisions
- A criterion-linked Verified Completion briefing
- Assignment-scoped resource Blockers that preserve Control Plane availability
- Public ordered lifecycle events, including Assignment readiness before dispatch
- Black-box end-to-end coverage through the CLI, protocol, daemon, and real restarts

The deterministic Worker echoes the accepted Goal. Each `exact_match` criterion compares its expected value with that candidate Result. It is a test configuration, not a production Agent Harness adapter. Tyrion rejects a proposal whose deterministic Result cannot fit its storage ceiling and rechecks attempt, elapsed-time, concurrency, and storage ceilings immediately before dispatch.

The Codex Git path accepts an explicit full base revision, authorized changed paths, and argv-based command verifiers. It records governing revisions, commits, paths, bundle artifacts, candidate and integrated verification outcomes, known effects, and the integrated artifact revision. See [Contained Codex Git assignments](docs/contained-codex.md) for its exact runtime contract and configuration.

## Run locally

Build the binaries:

```sh
cargo build
```

Start the Control Plane:

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock
```

Create `proposal.json`:

```json
{
  "goal": "return a deterministic greeting",
  "criteria": [
    {
      "id": "greeting",
      "description": "The Result contains the accepted greeting",
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

Issue a short-lived launch token for the expected adapter:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  attachment issue-token \
  --harness codex \
  --adapter-identity codex-mcp-entry \
  --adapter-version 1.0.0 \
  --idempotency-key issue-codex-token
```

Copy the returned token into the attachment handshake. A Full Entry Session currently advertises all seven supported capabilities:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  attachment connect \
  --token LAUNCH_TOKEN \
  --harness codex \
  --adapter-identity codex-mcp-entry \
  --adapter-version 1.0.0 \
  --native-session-id CODEX_SESSION_ID \
  --capability proposal_creation \
  --capability commission_acceptance \
  --capability commission_inspection \
  --capability event_replay \
  --capability control_takeover \
  --capability material_notifications \
  --capability persistent_mode_display \
  --idempotency-key connect-codex-session
```

Copy the returned `attachment_session_token`. This secret is returned once and authenticates the durable Attachment; the public Attachment ID shown in projections grants no authority. Review the proposal, then accept revision `0` through that Active Attachment:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  proposal create --file proposal.json --idempotency-key proposal-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission inspect COMMISSION_ID

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission accept COMMISSION_ID \
  --expected-revision 0 \
  --idempotency-key accept-1
```

Acceptance returns the durably committed active Commission and its ready Assignment. The daemon closes the response path before dispatch, continues authorized work with no Entry Session connected, and resumes any still-ready Assignment on restart. Resume by presenting the same session credential, adapter identity, native session identity, capability manifest, and last durable cursor:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  attachment resume COMMISSION_ID \
  --harness codex \
  --adapter-identity codex-mcp-entry \
  --adapter-version 1.0.0 \
  --native-session-id CODEX_SESSION_ID \
  --capability proposal_creation \
  --capability commission_acceptance \
  --capability commission_inspection \
  --capability event_replay \
  --capability control_takeover \
  --capability material_notifications \
  --capability persistent_mode_display \
  --last-event-sequence LAST_EVENT_SEQUENCE
```

A second Attachment may join a Commission as an observer by passing `--commission-id` and `--last-event-sequence` to `attachment connect`. It becomes the controller only after an explicit takeover:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token SECOND_ATTACHMENT_SESSION_TOKEN \
  commission take-control COMMISSION_ID \
  --expected-revision CURRENT_REVISION \
  --expected-control-revision CURRENT_CONTROL_REVISION \
  --idempotency-key transfer-control
```

Takeover does not change the Commission revision or Authority Envelope. It advances a separate control revision, records a durable `active_attachment_changed` event, promotes the requesting Attachment, and immediately demotes the former controller. Only the current Active Attachment can mutate the Commission or receive material notifications.

Reusing an idempotency key with the identical mutation returns its original response. A new mutation against an obsolete revision fails with a structured `stale_revision` error.

## Verify

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

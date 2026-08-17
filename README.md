# Tyrion

Tyrion is a durable local Control Plane for software-building Commissions. This initial walking skeleton implements the complete deterministic lifecycle from [issue #2](https://github.com/aneesh-sathe/tyrion/issues/2): a Principal reviews a proposal, explicitly accepts it, and receives Verified Completion only after current deterministic Evidence passes.

## What exists

- A versioned JSON protocol over a permission-restricted Unix socket
- A single-owner local daemon with SQLite WAL persistence
- Explicit proposal and acceptance commands with revision preconditions
- Durable idempotency keys for mutating requests
- One replaceable deterministic local Worker configuration
- Criterion-linked deterministic Evidence and a completion briefing
- Public ordered lifecycle events, including Assignment readiness before dispatch
- Black-box end-to-end coverage through the CLI, protocol, daemon, and real restarts

The deterministic Worker echoes the accepted Goal. Each `exact_match` criterion compares its expected value with that candidate Result. It is a test configuration, not a production Agent Harness adapter.

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

Review the proposal, then accept revision `0`:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  proposal create --file proposal.json --idempotency-key proposal-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  commission inspect COMMISSION_ID

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  commission accept COMMISSION_ID \
  --expected-revision 0 \
  --idempotency-key accept-1
```

Reusing an idempotency key with the identical mutation returns its original response. A new mutation against an obsolete revision fails with a structured `stale_revision` error.

## Verify

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

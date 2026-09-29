# OpenCode

OpenCode is Tyrion's fourth Worker harness. `adapters/opencode_server.py` runs
one Assignment against a real OpenCode server inside the same
`docker-hardened-v1` container every other Worker uses, and meets the same
structured contract: a revision-bound launch, streamed lifecycle and usage, a
typed Result, one terminal state, steering, and semantic interruption.

## How it runs

The pinned binary (1.18.32) is built into the Worker image at
`/opt/tyrion/harness/opencode`, and Tyrion probes its version inside each
sandbox before launch. The adapter starts `opencode serve` on loopback with a
random per-Attempt password, then:

- creates a session and sends the Assignment with `POST /session/{id}/prompt_async`
- follows progress live from the `GET /event` stream, filtered to that session:
  `message.part.updated` step, text and tool parts become Worker activity, and
  `session.idle` after `busy` ends the turn
- delivers steering as a further `prompt_async`, which OpenCode queues into the
  running turn
- interrupts with `POST /session/{id}/abort`, and treats the resulting
  `MessageAbortedError` as the expected consequence rather than a failure
- reads authoritative usage from `GET /session/{id}/message` after the turn,
  which also covers an interrupted turn

`opencode run --attach` was tried first and rejected: it buffers its output
until exit and does not exit after an abort.

## Sign-in and egress

OpenCode's ChatGPT sign-in uses the same OAuth client as Codex, so the Codex
login the Principal already approved serves both. At dispatch Tyrion reads
`codex_auth_file`, copies only the access token, refresh token, account ID and
the token's own expiry into OpenCode's `auth.json` shape, and streams it from
memory into the sandbox at mode 600. The runtime configuration names the file;
no credential value appears in it. The OpenCode profile is refused at startup
without `codex_auth_file`.

Egress is the Codex pair and nothing else: `chatgpt.com:443` for the model and
`auth.openai.com:443` for token refresh, each behind its own pinned relay.
OpenCode's model catalogue fetch to `models.dev` is denied and it falls back to
the snapshot it ships.

## What it does not do

The adapter delivers no native Skills. Its catalog entry does not claim the
`skills` capability and has an empty Skill inventory, so routing never sends a
Skill-requiring Assignment to it; if one arrives anyway the adapter fails with
a typed Required Skill failure. It declares no monetary budget, like Codex.

## Observed real behaviour

- The root filesystem is read-only, so OpenCode needs `TMPDIR` under
  `/sandbox`; without it the binary cannot even print its version.
- It reports no providers until it has a sign-in.
- A ChatGPT account rejects `gpt-5.4-mini`; `openai/gpt-5.6-sol` works.
- Real proof, 2026-09-27: two OpenCode and two Codex Workers ran one ledger
  plan concurrently and reached `verified_complete` in 63 seconds against 200
  seconds of serial Worker time, with no failed Evidence.
  [Record](../proof/2026-09-27-opencode-cross-harness.json).

## Running the real test

```sh
tyrion init
TYRION_REAL_WORKER_RUNTIME=~/.local/state/tyrion/runtime/worker-runtime.json \
TYRION_REAL_WORKER_CATALOG=~/.local/state/tyrion/runtime/worker-catalog.json \
cargo test --test git_commission real_opencode -- --ignored
```

The default suite uses `tests/fixtures/fake_structured_adapter.sh`, whose
OpenCode branch mirrors the event shapes the real server produced and checks
that Tyrion delivered the login in OpenCode's own format.

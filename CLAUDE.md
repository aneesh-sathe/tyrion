# Project notes

- Domain language follows the issue tracker: Principal, Commission Proposal, Commission, Acceptance Criterion, Authority Envelope, Assignment, Attempt, Result, Evidence, Control Plane, and Verified Completion.
- The public seam is the `tyrion` CLI over the versioned Unix-socket protocol to `tyriond`. End-to-end tests must observe only that seam and must use real SQLite state and daemon restarts.
- `tyriond` is the only authoritative writer. It holds an exclusive lock per data directory, protects the directory and socket with user-only permissions, and uses `state.sqlite3` in WAL mode.
- Proposal creation grants no execution authority. Acceptance requires `deterministic.echo` in the Authority Envelope plus an exact expected revision and idempotency key.
- `deterministic-local-v1` echoes the accepted Goal. It is a replaceable test Worker configuration, not a production harness adapter.
- Evidence is immutable and bound to criterion, accepted mandate revision, candidate Result, verifier, and artifact revision. Failed Evidence leaves a candidate Result unaccepted.
- Verified Completion, accepted Result status, passed criteria, completion briefing, and the terminal event are committed in one SQLite transaction.
- Mutating protocol requests require idempotency keys. Identical replay returns the stored response; key reuse with a different request is rejected.
- Run `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test` before committing.
- A stale socket remains after forced test shutdown. Startup may replace a socket file only after acquiring data-directory ownership and must refuse to replace any non-socket path.

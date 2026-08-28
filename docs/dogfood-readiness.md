# Dogfood readiness proof

Issue 16 requires one substantive Git-backed Commission to combine useful cross-harness concurrency, integrated verification, a controlled consequential effect, planned recovery, applied preference context, and a reproducible terminal record.

## Safe smoke result

On 2026-08-28, the public CLI and daemon seam completed the smallest safe fixture-backed smoke available inside this repository:

- Codex and Claude contract fixtures received two disjoint Git Assignments concurrently.
- Tyrion verified both candidate Results, integrated them in dependency order, verified the assembled artifact, and reached Verified Completion.
- `commission export-record` returned a checksummed record containing the mandate, routes, Attempts, Results, Evidence, Integration, events, learning receipts, and the final run report.
- A separate daemon-restart scenario preserved the failed Attempt, confirmed cleanup, retried safely, and reported the recovery without misclassifying it as a failed Security Invariant.
- Structured fixture worktrees stayed inside the test workspace rather than using a hard-coded host temporary path.

This is product and record-format verification only. Fixture-backed Worker evidence is not production containment attestation, and separate scenarios are not the single real Commission required by issue 16.

## Current actionable Blocker

Do not claim dogfood readiness until one small production Commission runs with an already provisioned, pinned repaired OpenShell runtime and eligible real Worker providers. The run must use the exact production Worker configurations in its exported record and combine all issue 16 criteria in that one Commission.

The current safe implementation run did not inspect credentials, access host runtime state outside this repository, build virtualization artifacts, reconfigure the gateway, or perform a network effect. Under that boundary, production containment and real cross-harness execution are unavailable. The smallest next requirement is:

> Supply explicit permission and paths for an already provisioned pinned Worker runtime and provider configuration, then run one small Commission whose Principal checkout remains read-only and whose only consequential effect targets a disposable local file.

If those prerequisites are unavailable, preserve this Blocker and leave issue 16 open. Do not substitute fixture evidence or tests for the missing production run.

## Record review

Export the terminal Commission through its authenticated Entry Session:

```sh
target/debug/tyrion --socket "$TYRION_SOCKET" \
  --attachment-token "$ATTACHMENT_SESSION_TOKEN" \
  commission export-record "$COMMISSION_ID" > commission-record.json
```

Recompute the record checksum independently:

```sh
jq -cjS .record commission-record.json | shasum -a 256
```

Review these fields before making a readiness claim:

- `.record.commission` for Goal, versioned mandate inputs, Authority Envelope, resource ceilings, repository identity, artifact identity, and uncertainties.
- `.record.assignments`, `.record.attempts`, and `.record.workers` for useful conflict-free routing, exact Worker configurations, containment profiles, and resource reservations.
- `.record.results` and `.record.evidence` for candidate verification, dependency-ordered Integration, assembled verification, and current criterion bindings.
- `.record.operation_requests` and `.record.approval_gates` for the exact controlled effect and single-use authorization path.
- `.record.restart_recoveries`, `.record.recovery_history`, and `.record.events` for the planned failure, cleanup, replay, and continued work.
- `.record.briefing.learning_receipts` for advisory Profile Claim application.
- `.record.briefing.run_report` for separate Approval Gate, intervention, correction, context-transfer, reconciliation, concurrency, conflict, cost, timing, failure, and recovery metrics.

Any nonzero `.record.briefing.run_report.failures.security_invariant_failures` blocks readiness until the corresponding claim is explicitly withdrawn or a new production record demonstrates the invariant.

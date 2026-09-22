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

The captured smoke export is checked in at [`dogfood-records/2026-08-28-safe-smoke.json`](dogfood-records/2026-08-28-safe-smoke.json). Its `dogfood_readiness.status` is `blocked` with the `fixture_backed_evidence` reason. Recreate a fresh fixture-backed record without accessing production credentials or host runtime state:

```sh
mkdir -p .scratch/test-tmp
TMPDIR="$PWD/.scratch/test-tmp" \
TYRION_CAPTURE_COMMISSION_RECORD="$PWD/.scratch/issue-16-safe-smoke.json" \
  cargo test --test git_commission \
  codex_and_claude_structured_adapters_complete_one_git_commission \
  -- --exact
```

## Current actionable Blocker

2026-09-21 containment replacement: plain Docker replaced the repaired
OpenShell MicroVM as the Worker containment boundary, removing Tyrion's
custom-kernel distribution burden. Every ceiling the profile requires is now
enforced by the Docker daemon from outside the container and was measured,
including the 256-process ceiling stock Docker Sandboxes could not enforce.
[Qualification and complete claim set](prototypes/docker-containment-qualification.md).
This is a no-model boundary qualification. No real Worker has executed on it,
so it is not a dogfood readiness claim and issue 16 remains open.

Two contract changes need an explicit Principal decision before production
eligibility changes:

1. **Resources.** 2 vCPU / 2048 MiB memory / 4096 MiB overlay became 2 vCPU /
   6144 MiB combined memory-and-files / 4096 MiB writable storage / 256
   processes. Same host envelope, one hard combined ceiling plus a hard
   storage sub-ceiling, because the memory cgroup charges tmpfs pages.
2. **Credentials.** A declared provider credential now reaches the Worker
   execution's environment, scoped to one `docker exec`. The OpenShell provider
   kept it out of the Attempt entirely. Destination pinning still prevents
   sending it elsewhere; it does not bound spend or disclosure at the
   destination.

2026-09-11 Docker Sandboxes qualification: rejected. No process-ceiling or
storage flag exists, `--allow-network` and `--ttl` are cloud-only, `--skills`
defaults to a writable mount, and `--clone` bind-mounts the host repository.
[Results and limits](prototypes/docker-sandboxes-qualification.md) are retained
as evidence.

Do not claim dogfood readiness until one small production Commission runs on
the Docker boundary with eligible real Worker providers. The run must use the
exact production Worker configurations in its exported record and combine all
issue 16 criteria in that one Commission. The smallest next requirement is:

> Provision the pinned Worker image and the Linux Codex binary, name the
> authorized provider, model, credential variable, and spend limit, then run
> one small Commission whose Principal checkout remains read-only and whose
> only consequential effect targets a disposable local file.

If those prerequisites are unavailable, preserve this Blocker and leave issue
16 open. Do not substitute fixture evidence or a boundary qualification for the
missing production run.

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
- `.record.run_report` for separate Approval Gate, mandate-bound planned control, unplanned intervention, correction, context-transfer, reconciliation, concurrency, conflict, cost, timing, failure, and recovery metrics. Verified Completion also copies this report into `.record.briefing.run_report`.
- `.dogfood_readiness` for the fail-closed readiness assessment. `blocked` names machine-readable blockers; `unassessed` still requires a Principal review of every issue 16 criterion. The exporter never emits `ready`.

Any nonzero `.record.run_report.failures.security_invariant_failures` forces `.dogfood_readiness.status` to `blocked` until the corresponding claim is explicitly withdrawn or a new production record demonstrates the invariant. Containment preflight failures use the explicit `security_invariant_failure` blocker code rather than message-text classification.

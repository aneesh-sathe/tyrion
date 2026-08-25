# Tyrion

Tyrion is a durable local Control Plane for software-building Commissions. The walking skeleton implements the deterministic lifecycle from [issue #2](https://github.com/aneesh-sathe/tyrion/issues/2), the durable Entry Session attachment contract from [issue #3](https://github.com/aneesh-sathe/tyrion/issues/3), the contained Codex Git path from [issue #4](https://github.com/aneesh-sathe/tyrion/issues/4), the verification and rework kernel from [issue #5](https://github.com/aneesh-sathe/tyrion/issues/5), useful conflict-aware parallel Assignments from [issue #6](https://github.com/aneesh-sathe/tyrion/issues/6), inspectable cross-harness Worker routing and control from [issue #7](https://github.com/aneesh-sathe/tyrion/issues/7), exact consequential-effect authorization from [issue #9](https://github.com/aneesh-sathe/tyrion/issues/9), and credentialed broker and one-shot effects from [issue #10](https://github.com/aneesh-sathe/tyrion/issues/10). A Principal connects an explicit Entry Session, reviews and accepts a proposal through the Active Attachment, and receives Verified Completion only after current integrated Evidence passes.

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
- Deterministic, model, and Principal verifiers with standard or independent depth
- Immutable Evidence bound to the mandate, artifact, verifier configuration, procedure, and environment
- Daemon-issued verification Attempt identities bound to the authenticated Attachment
- Durable Principal verification gates that must close before completion
- Visible passed, failed, and uncertain verdicts with one derived recovery action
- Durable verification recovery records with pending, scheduled, attention, blocked, and resolved states
- Result rework that retains prior Attempts and marks superseded Evidence stale
- Revision-checked verification amendments with retained criterion history
- Incremental versioned Commission Plans with dependency-derived Execution Frontiers
- Explicit critical-path, uncertainty-reduction, and independent-verification dispatch purposes
- Transactional concurrency, storage, model-spend, and paid-service reservations per Attempt
- Concurrent read and disjoint-write execution with declared overlap serialization
- Explicit competing work with a retained uncertainty and comparison rule
- Serialized dependency-aware Git Integration from immutable current artifact revisions
- Explicit reconciliation for unexpected scope, conflict, stale base, and assembled regressions
- Activity Journal timing Evidence for verified useful concurrency
- Complete Worker Configuration routing across Codex and Claude without Entry Session affinity
- Hard capability, authority, tool, Skill, context, Assignment, and resource eligibility gates
- Stable Commission-local Worker Handles with inspection, steering, and interruption
- Cause-derived recovery with one bounded same-configuration retry, immediate poor-fit rerouting, and replanning after a second equivalent failure
- Revision dispositions that retain superseded, stale, and revalidation-required Attempts and Results without allowing stale Integration
- A daemon Watchdog that contains the narrowest stalled, unhealthy, over-budget, non-live, or unauthorized Attempt
- Fail-closed restart reconciliation with explicit identity, acknowledgement, lease, authority, and containment proofs
- Durable pause, resume, and Principal cancellation with lease and reservation revocation
- Exact operation classification, independent Principal Approval Gates, and single-use effect authorization
- macOS Keychain-backed credential grants with brokered stdin delivery or a fresh one-shot Effect Sandbox
- Revision-bound Commission Amendments with exact diffs and affected-work revalidation
- Actionable resumable Blockers containing criteria, Evidence, artifacts, failed approaches, resource use, and the exact next requirement
- Structured Codex app-server and Claude Agent SDK lifecycle contract validation
- Approximately equal replacement or a durable Attention Condition when a preferred configuration is unavailable
- A criterion-linked Verified Completion briefing
- Assignment-scoped resource Blockers that preserve Control Plane availability
- Public ordered lifecycle events, including Assignment readiness before dispatch
- Black-box end-to-end coverage through the CLI, protocol, daemon, and real restarts

The deterministic Worker echoes the accepted Goal. Each `exact_match` criterion compares its expected value with that candidate Result. It is a test configuration, not a production Agent Harness adapter. Tyrion rejects a proposal whose deterministic Result cannot fit its storage ceiling and rechecks attempt, elapsed-time, concurrency, and storage ceilings immediately before dispatch.

The Codex Git path accepts an explicit full base revision, authorized changed paths, and argv-based command verifiers. It records governing revisions, commits, paths, bundle artifacts, candidate and integrated verification outcomes, known effects, and the integrated artifact revision. See [Contained Codex Git assignments](docs/contained-codex.md) for its exact runtime contract and configuration.

An optional `plan` on a `codex_git` proposal enables multiple Assignments. Every planned Assignment declares dependencies, owned Acceptance Criteria, one useful dispatch purpose, read and write scopes, and its complete resource reservation. Read-only Assignments can verify without changing the authoritative artifact. Overlapping writes remain held unless both entries share the same non-empty `competition` group, uncertainty, and comparison rule. Plans currently require deterministic command Evidence so Tyrion can independently verify each candidate and the assembled repository.

```json
{
  "plan": {
    "assignments": [
      {
        "id": "backend",
        "goal": "Implement the backend change",
        "dependencies": [],
        "criterion_ids": ["backend-check"],
        "purpose": "critical_path",
        "read_scopes": ["contracts"],
        "write_scopes": ["src/backend"],
        "resources": {
          "concurrency_slots": 1,
          "max_storage_bytes": 5242880,
          "max_model_spend_cents": 0,
          "max_paid_service_spend_cents": 0
        }
      },
      {
        "id": "frontend",
        "goal": "Implement the frontend change",
        "dependencies": [],
        "criterion_ids": ["frontend-check"],
        "purpose": "critical_path",
        "read_scopes": ["contracts"],
        "write_scopes": ["web/frontend"],
        "resources": {
          "concurrency_slots": 1,
          "max_storage_bytes": 5242880,
          "max_model_spend_cents": 0,
          "max_paid_service_spend_cents": 0
        }
      }
    ]
  }
}
```

The Control Plane validates the whole dependency graph, criterion ownership, cumulative spend, and any comparison-attempt allowance before proposal creation. Competition members must share one dependency frontier and cannot depend on each other, and their worst-case comparison working set must fit the storage ceiling. Acceptance records the entry-model plan, exposes only a safe and fully reservable frontier, and commits every dispatch reservation atomically with its Attempt. Current Evidence and comparison or rework Assignments create later plan revisions through the same safe-frontier selector. Accepted Results integrate one at a time. Competing candidates remain unintegrated until a comparison Assignment receives every contender bundle and its Evidence, applies the declared rule, and produces a fresh verified Result. Generic reconciliation reserves the Commission storage ceiling plus the source Assignment's compute and spend budgets before inspecting its candidate. A failed assembled-state check retains its Evidence, rolls the integration worktree back, and records dispatchable rework. Verified Completion requires every ordinary planned Assignment to be accepted or explicitly superseded by reconciliation and every criterion to pass on the final artifact revision. The Activity Journal uses the union of accepted and explicitly planned contender execution intervals to retain serial execution time, the parallel execution window, measured reduction, and final end-to-end elapsed time.

Start `tyriond` with `--worker-catalog <json>` to route each ready Assignment as one complete Worker Configuration. Proposal-level exact Skill Versions are inherited by every explicit Assignment; its other Worker requirements remain plan-specific. Compatible Assignment-level exact Skill Versions may be added, while conflicting Principal and plan Skill identities fail closed. Eligible configurations are ordered lexicographically by expected verified correctness, measured preference adherence, first-pass acceptance, elapsed-time contribution, cost, continuity, and finally stable configuration ID. The Entry Session harness is retained only in the visible rationale and never changes that order. See [Cross-harness Worker routing and control](docs/cross-harness-workers.md).

Available Codex app-server and Claude Agent SDK configurations name an absolute, SHA-256-pinned adapter executable. Tyrion launches each with its own pinned native CLI, brokered provider, and endpoint-restricted OpenShell policy, plus a structured revision-bound Assignment and spend envelope. It validates the exact native model, tool and Skill preparation plus invocation state, lifecycle, usage, and typed Result; forwards journal-first controls; and persists native session telemetry for inspection. Launch-time unavailability reuses the approximately-equal routing rule, while durable cleanup survives repeated Control Plane crashes. Git-backed adapters receive only Tyrion-created bundle bindings; Tyrion commits ordinary uncommitted harness edits before independently validating and integrating the candidate. Production reference adapters are in `adapters/`.

Credentialed effects keep secret values in macOS Keychain and outside SQLite, Worker environments, Entry Sessions, command arguments, logs, Evidence, and durable receipts. The daemon binds a Credential Grant and Principal Approval Gate to the exact current authority, Assignment, Attempt, Worker Lease, revisions, destination, parameters, consequences, and resource limits. Typed HTTP effects use the broker directly. Exceptional exposure creates a fresh non-agentic repaired OpenShell sandbox, delivers the secret once over standard input, terminates descendants, destroys the sandbox, revokes the Keychain item, and records only a redacted receipt. See [Credentialed effects](docs/credentialed-effects.md).

## Run locally

Build the binaries:

```sh
cargo build
```

Start the Control Plane, optionally with a complete Worker catalog:

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config runtime.json \
  --worker-catalog worker-catalog.json \
  --credential-runtime credential-runtime.json
```

Create `proposal.json`:

```json
{
  "project_id": "project-tyrion",
  "goal": "return a deterministic greeting",
  "commission_constraints": [
    "Return the exact accepted greeting for this Commission only."
  ],
  "worker_requirements": {
    "capabilities": ["structured_lifecycle", "semantic_interrupt"],
    "tools": [],
    "skills": [],
    "min_context_tokens": 0,
    "context_strategy": "fresh",
    "assignment_constraints": []
  },
  "criteria": [
    {
      "id": "greeting",
      "description": "The Result contains the accepted greeting",
      "required_evidence": "exact_output",
      "verifier_type": "deterministic",
      "verification_depth": "standard",
      "verifier_configuration": "deterministic-exact-match-v1",
      "verification_environment": "tyrion-controlled-v1",
      "verifier": {
        "kind": "exact_match",
        "expected": "return a deterministic greeting"
      }
    }
  ],
  "authority": {
    "repositories": ["."],
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

Protocol v2 represents each required native Skill as `{ "name", "content_digest": "sha256:..." }`. Optional `selected_skills` wrap that object as `version` and carry `principal` provenance at Commission scope or `plan` provenance inside an explicit Assignment. Worker catalog entries use flat exact Skill Version objects and may declare Worker-provenance `selected_skills` defaults. See `docs/cross-harness-workers.md` for package hashing, native delegation, failure recovery, and Result association semantics.

Issue a short-lived launch token for the expected adapter:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  attachment issue-token \
  --harness codex \
  --adapter-identity codex-mcp-entry \
  --adapter-version 1.0.0 \
  --idempotency-key issue-codex-token
```

Copy the returned token into the attachment handshake. A Full Entry Session currently advertises all nine supported capabilities:

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
  --capability worker_steering \
  --capability worker_interruption \
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
  --capability worker_steering \
  --capability worker_interruption \
  --last-event-sequence LAST_EVENT_SEQUENCE
```

Pause stops new dispatch without changing the accepted mandate or discarding resumable work. Resume restarts safe frontier dispatch. Principal cancellation is terminal for that Commission: it revokes active Worker Leases and resource reservations while retaining history, integrated artifacts, and already-made effects.

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission pause COMMISSION_ID \
  --expected-revision CURRENT_REVISION \
  --idempotency-key pause-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission resume COMMISSION_ID \
  --expected-revision CURRENT_REVISION \
  --idempotency-key resume-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission cancel COMMISSION_ID \
  --expected-revision CURRENT_REVISION \
  --idempotency-key cancel-1
```

Workers propose operations through the Active Attachment. The daemon classifies each request as `silent_journaled`, `non_blocking_notification`, `approval_gate`, or `prohibited`. Classification is bound to the current Commission and plan revisions, Assignment, Attempt, Worker Lease, exact repository and target, parameters, destination, effect, consequences, and limits. Capability declarations, installed tools, ambient machine access, prior approvals, and requested outcomes never add authority.

A consequential `filesystem.write` remains blocked until the Principal approves its canonical digest through an ephemeral control credential. A trusted launcher may create an anonymous private pipe, inherit its write end as a dedicated descriptor numbered 3 or greater, and pass that number with `--principal-control-bootstrap-fd`. The daemon rejects absent and non-pipe descriptors, writes one `TYRION_PRINCIPAL_CONTROL_TOKEN=...` line, closes the descriptor immediately, retains only the hash, and never writes the secret to disk or sends it to standard output, an Attachment, or a Worker. Without that option, Principal control remains deliberately unavailable. The launcher must keep the read end out of logs, repositories, harness environments, and Worker sandboxes. After the launcher obtains the token, a typical flow is:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  operation propose COMMISSION_ID \
  --file operation.json \
  --expected-revision CURRENT_REVISION \
  --idempotency-key propose-effect-1

printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --principal-token-stdin principal inspect-gate APPROVAL_GATE_ID

printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --principal-token-stdin principal approve-gate COMMISSION_ID APPROVAL_GATE_ID \
  --expected-operation-digest OPERATION_DIGEST \
  --expected-revision CURRENT_REVISION \
  --idempotency-key approve-effect-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  operation execute COMMISSION_ID APPROVAL_GATE_ID \
  --file operation.json \
  --expected-revision CURRENT_REVISION \
  --idempotency-key execute-effect-1
```

Execution accepts only the canonical request represented by the approved digest, including the canonical repository, directory and target identities, and before-content digest. It consumes the gate once and records a redacted receipt. Changed parameters, target identity or content revision, destination, effect, mandate revision, or replay are rejected. The current implementation uses descriptor-relative, no-follow access and atomic replacement of an existing regular file within the exact authorized repository and path.

Replacement moves the current target aside without overwriting anything, verifies that the moved inode and digest are exactly those approved, then installs the prepared file with an atomic no-replace rename. A concurrent target change therefore fails closed rather than being overwritten. If restart recovery cannot prove whether a started effect committed, Tyrion marks it uncertain and pauses the Commission. The Principal must independently observe the exact target digest and reconcile it before the Commission can resume:

```sh
printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --principal-token-stdin principal reconcile-operation \
  COMMISSION_ID OPERATION_REQUEST_ID \
  --outcome confirmed \
  --observed-sha256 OBSERVED_TARGET_SHA256 \
  --expected-revision CURRENT_REVISION \
  --idempotency-key reconcile-effect-1
```

Use `--outcome not-applied` only when both Principal observation and Tyrion's independent no-follow observation match the approved before-content digest. Execution replay after recovery returns the stored uncertain projection rather than attempting the effect again.

Authority or ceiling expansion uses `commission propose-amendment` with the complete replacement Authority Envelope and resource ceilings. The Principal inspects the exact diff with `principal inspect-amendment` and accepts its digest with `principal accept-amendment`. Acceptance advances the mandate revision, invalidates pending effect grants, and revalidates every active Worker Lease. Operations approaching the storage ceiling notify the Active Attachment; operations beyond it are prohibited until an Amendment is accepted. Cancellation revokes pending grants and Leases, marks started effects uncertain, and retains confirmed irreversible receipts.

Commission inspection exposes `recovery_history`, `restart_recoveries`, `watchdog`, and a derived `recovery` briefing. When no useful safe frontier remains, `recovery.resumable_blocker` names passed and unresolved criteria, retained artifacts and Evidence, failed approaches, cumulative resource use, and the exact requirement for progress.

Inspection returns every Worker Handle, exact configuration, routing rationale, Assignment, elapsed time, latest meaningful activity, usage, and currently available controls. The Active Attachment can clarify or stop a running Worker without changing the Commission mandate revision:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  worker steer COMMISSION_ID Arya \
  --clarification "Keep the accepted API contract unchanged." \
  --expected-revision CURRENT_REVISION \
  --idempotency-key steer-arya-1

target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  worker interrupt COMMISSION_ID Arya \
  --reason "Stop this Attempt." \
  --expected-revision CURRENT_REVISION \
  --idempotency-key interrupt-arya-1
```

Model and Principal criteria remain visibly `uncertain` after Integration until the Active Attachment records matching structured Evidence. Each `evidence.json` record names its criterion, current Result, Evidence type, verdict, exact verifier configuration, procedure, environment, inspectable output, and any diagnosed defect. The daemon creates the Verification Attempt identity and binds the verifier identity to the authenticated Attachment, so caller-supplied identity claims are rejected. Failed or uncertain Evidence must classify the defect as `result`, `verifier`, `environment`, or `criterion`.

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission record-evidence COMMISSION_ID \
  --file evidence.json \
  --expected-revision CURRENT_REVISION \
  --idempotency-key record-review-1
```

A result defect routes rework while Attempts remain. Environment retry records a fresh daemon-issued Verification Attempt, verifier reroute can transfer control to another eligible Attachment, criterion escalation can enter the revision-checked amendment path, and exhausted attempt ceilings produce an actionable Blocker. Each diagnosis creates a durable `verification_recoveries` record whose state survives restart and resolves when the routed action starts or replacement Evidence arrives. Principal criteria also create durable verification gates that remain open until sufficient current Principal Evidence from distinct verifier Attachments passes. A verification-only mandate change uses an explicit amendment containing the complete current criterion set:

```sh
target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  commission amend-verification COMMISSION_ID \
  --file amendment.json \
  --expected-revision CURRENT_REVISION \
  --idempotency-key amend-verification-1
```

The amendment cannot change the Goal, Authority Envelope, resource ceilings, or criterion identifiers. It retains prior criterion versions and Evidence, marks old Evidence stale, and routes a fresh Attempt under the new mandate revision.

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

## Reusable Principal preferences

A reusable software-building preference requires both the Active Attachment credential for its source Commission and the independent Principal control credential. An explicit `remember-preference` creates one atomic hard Profile Claim. If the source proposal names a `project_id`, its Authority Envelope must include repository Evidence. Tyrion binds each canonical repository device and inode before giving the claim narrow project scope. A later proposal can add repositories or worktrees only when it also presents an already-bound identity as an anchor; moves on the same filesystem can add a verified path alias. Without a `project_id`, the claim receives Principal scope. `commission_constraints` remain binding only within their Commission and never become Profile Claims automatically.

```sh
printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  --principal-token-stdin \
  principal remember-preference COMMISSION_ID \
  --statement "Prefer behavior-first tests at public seams." \
  --idempotency-key remember-behavior-first-tests
```

The response includes the complete versioned claim and a compact `profile_claim_created` Learning Receipt. The Principal can later inspect the claim, its provenance, and every affected Attempt, or inspect the active claims applicable to a project:

Eligible Result outcomes can also create inferred soft candidates. One strong observation creates a project candidate. Two independent Commissions can promote it only when at least one observation is a Principal edit or an explained rejection. An unedited acceptance alone can neither originate nor promote a claim. Support spanning at least three Commissions and two projects creates a Principal-scope candidate, but only explicit Principal confirmation activates it. Material contradiction deactivates applicable soft claims, and soft claims without non-weak support for 180 days visibly decay back to candidate state. Lifecycle transitions and immutable observations remain inspectable.

```sh
printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  --principal-token-stdin \
  principal observe-preference COMMISSION_ID \
  --statement "Prefer behavior-first tests at public seams." \
  --outcome principal-edit \
  --idempotency-key observe-behavior-first-tests

printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  --principal-token-stdin \
  principal confirm-preference COMMISSION_ID CLAIM_ID \
  --expected-version 1 \
  --idempotency-key confirm-behavior-first-tests
```

```sh
PREVIEW=$(printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  --principal-token-stdin \
  principal revise-preference COMMISSION_ID CLAIM_ID \
  --statement "Prefer end-to-end tests at public seams." \
  --expected-version 1 \
  --idempotency-key preview-behavior-first-tests)

DIGEST=$(printf '%s' "$PREVIEW" | jq -r .confirmation_digest)

printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --attachment-token ATTACHMENT_SESSION_TOKEN \
  --principal-token-stdin \
  principal revise-preference COMMISSION_ID CLAIM_ID \
  --statement "Prefer end-to-end tests at public seams." \
  --expected-version 1 \
  --confirmation-digest "$DIGEST" \
  --idempotency-key revise-behavior-first-tests
```

Without a confirmation digest, revision returns the exact diff and its digest without changing the claim. Confirming that digest against the expected current version appends an immutable superseding claim version and returns a `profile_claim_changed` receipt. An explicit correction can resolve a contradicted claim while retaining the contradiction in lifecycle history. Suppression deactivates the claim without deleting history. Forgetting first previews its exact cascade and confirmation digest, then atomically removes the claim, its versions, dedicated observations and excerpts, Attempt links, indexes, and caches. The resulting deletion receipt contains identifiers and counts but no forgotten content. `prevent-preference` adds a scoped Learning Boundary that blocks later inference, confirmation, active import, and reactivation for the same statement.

```sh
printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --principal-token-stdin principal inspect-claim CLAIM_ID

printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket .scratch/tyrion-data/tyrion.sock \
  --principal-token-stdin principal inspect-profile \
  --project-id project-tyrion
```

Retrieval is deterministic. It selects only active software-building claims whose Principal or verified project scope applies, excludes claim versions created from the current Commission, and merges them in scope, strength, and creation order. Admission demotes or evicts lower-priority soft memory before rejecting a hard claim, and never truncates a claim. Advisory memory targets 2,000 tokens and is capped at the smaller of 15,000 tokens or 8 percent of the selected Worker Configuration's context capacity. Tyrion uses the serialized UTF-8 byte length as a conservative cross-tokenizer upper bound, including all injected claim metadata. Principal and Project Profiles also enforce their independent active claim and token limits.

`export-memory` produces a versioned, checksummed JSON bundle plus a readable Markdown summary for either Principal or project scope. It includes claim versions, lifecycle and observation history, transitive Commission provenance, Learning Boundaries, and content-free deletion receipts. It excludes credentials, session material, and raw secrets. `import-memory` validates a strict field whitelist, checksum, complete audit histories, scope, and prohibited fields before restoring the bundle with its original provenance. Imported source project identifiers remain provenance and never establish local repository identity.

Temporary raw transcript and unaccepted artifact material receives a 30-day terminal retention deadline unless it is pinned, supports Evidence or a claim, or its effect remains uncertain. Active material never expires. Structured Commission Records and Evidence are retained after temporary content expires.

Every Attempt persists its exact `worker_context_packet`. Current Principal instructions, Commission constraints, Acceptance Criteria, Authority Envelope, resource ceilings, and current repository Evidence appear as binding sources ahead of advisory Profile Claims. A claim cannot affect routing eligibility, Approval Gates, credentials, or resource ceilings. Structured adapters receive the same packet in `tyrion.assignment.launch`.

Each influenced Result projects its claim version outcome as `accepted`, `edited`, `rejected`, or `contradicted`. A rejected Result changes to `edited` only after a replacement Result is produced for the same Assignment. Verified Completion includes compact Learning Receipts for claims created or changed by that Commission and for both successful and unsuccessful applications.

## Verify

```sh
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

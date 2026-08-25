# Cross-harness Worker routing and control

Tyrion routes an Assignment to a complete Worker Configuration. The routing unit includes the harness, adapter kind and version, model, all model settings, tools, native Skills, context strategy and capacity, resource limits, capabilities, authority actions, Assignment constraints, containment profile, replacement class, availability, and inspectable metrics. The daemon rejects incomplete catalogs and duplicate configuration IDs at startup.

Pass the catalog to the daemon:

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config runtime.json \
  --worker-catalog worker-catalog.json
```

A catalog has this shape:

```json
{
  "configurations": [
    {
      "id": "claude-opus-review",
      "harness": "claude",
      "adapter": {
        "kind": "claude_agent_sdk",
        "version": "0.2.0",
        "sha256": "REPLACE_WITH_ADAPTER_SHA256",
        "command": ["/opt/tyrion/bin/claude-agent-sdk-adapter"]
      },
      "model": "claude-opus-5",
      "settings": {
        "effort": "high"
      },
      "tools": ["git"],
      "skills": [{
        "name": "code-review",
        "content_digest": "sha256:REPLACE_WITH_64_LOWERCASE_HEX_DIGITS"
      }],
      "selected_skills": [],
      "context": {
        "strategy": "fresh_with_retrieval",
        "capacity_tokens": 200000
      },
      "resource_limits": {
        "max_concurrency_slots": 1,
        "max_storage_bytes": 2097152,
        "max_model_spend_cents": 200,
        "max_paid_service_spend_cents": 0
      },
      "capabilities": [
        "structured_lifecycle",
        "semantic_interrupt",
        "terminal_state",
        "usage",
        "skills",
        "result_submission",
        "contained"
      ],
      "authority_actions": ["codex.git_change"],
      "authority_scope_types": ["repository", "path", "action"],
      "assignment_constraints": ["coding"],
      "containment_profile": "openshell-repaired-v0.0.104",
      "replacement_class": "deep-coding",
      "available": true,
      "metrics": {
        "expected_verified_correctness": 9500,
        "preference_adherence": 9100,
        "first_pass_acceptance": 9200,
        "commission_elapsed_time_contribution_ms": 1400,
        "cost_cents": 90,
        "continuity": 1000
      }
    }
  ]
}
```

Score fields other than elapsed time and cost are basis points from 0 through 10,000. A configuration marked available is eligible only after every hard gate passes. Required or excluded configuration IDs, required capabilities, the Assignment's authority action, tools, Skills, context strategy and minimum capacity, Assignment constraints, and the complete reserved resource vector are gates, not ranking inputs.

Every native Skill inventory entry names one exact `sha256:` content identity. The digest covers the sorted package-relative file names, executable bits, byte lengths, and file contents rooted beside `SKILL.md`; symlinks and non-file package entries fail closed. `worker_requirements.skills` contains Principal-mandated Required Skill Versions. `worker_requirements.selected_skills` wraps the exact identity in `version` and may select it with `principal` provenance at Commission scope or `plan` provenance inside a planned Assignment. A Worker Configuration may declare flat `selected_skills` identities as its Worker-provenance first-invocation defaults, and each must exactly match its `skills` inventory. Optional selections are carried into the launch but persisted only after an exact native invocation is observed. Contract-validated invocation telemetry is recorded before terminal success, failure, or interruption handling, so the exact default remains across reroutes.

Acceptance pins Required Skill defaults to the Assignment and plan revision. The first observed optional invocation pins that exact default under the current plan revision. A retry or reroute must support all pinned defaults and cannot silently replace a digest. Inspection exposes each immutable `skill_default` with its required or selected role, provenance, plan revision, and `native_unchanged` delegation mode. Skill compatibility is only a hard routing gate; Tyrion does not rank, rewrite, translate, dependency-check, or substitute Skill packages.

Structured adapters support `fresh` and `fresh_with_retrieval`. Both create a new native session. `fresh` supplies the accepted Assignment context, while `fresh_with_retrieval` also directs the Worker to retrieve relevant workspace context with its configured tools and Skills before acting. Catalog startup and each production adapter reject any strategy they cannot honor. An Assignment may require one exact strategy with `worker_requirements.context_strategy`.

Tyrion orders the remaining configurations lexicographically:

1. Expected verified correctness
2. Measured preference adherence
3. First-pass acceptance
4. Lower Commission elapsed-time contribution
5. Lower cost
6. Higher continuity
7. Stable configuration ID

The active Entry Session harness is recorded in `route.rationale.entry_harness`. `entry_harness_preference_applied` is always false. Starting the same Commission from Codex or Claude therefore produces the same decision from the same catalog, requirements, and metrics.

If the first-ranked configuration is unavailable, Tyrion automatically selects another available eligible configuration only when it has the same replacement class and is no more than 100 basis points worse on expected verified correctness, preference adherence, or first-pass acceptance. Otherwise the Assignment enters `attention_required` and receives an open Attention Condition with the exact requirement for progress. Attention routing is evaluated again after a catalog change and daemon restart, so making the required configuration available resumes dispatch without rewriting the Commission.

## Adapter contract

The catalog may request `openshell-repaired-v0.0.104`; Tyrion replaces that alias with the exact pinned runtime-file fingerprint before routing, so inspection exposes the concrete containment configuration.

An available `codex_app_server` or `claude_agent_sdk` configuration must provide an absolute adapter command and the SHA-256 digest of its executable. Every structured configuration requires `--codex-worker-config`. Available Claude configurations additionally require the separately pinned `claude` runtime block shown in `runtime/openshell/codex-worker.example.json`. Tyrion verifies every adapter, CLI, and policy digest, creates and preflights the harness-specific OpenShell sandbox, attaches only that harness's brokered provider, and uploads only the adapter executable, its pinned native CLI, and Tyrion-created Git bundle inputs. The Codex policy permits only the pinned Codex binary to reach OpenAI endpoints. The Claude policy permits only the pinned Claude binary to reach Anthropic endpoints. Tyrion then starts the adapter through `openshell sandbox exec`. It supplies non-secret Commission, Assignment, Attempt, revision, artifact, and exact reserved resource bindings. The first JSON Lines message on stdin is `tyrion.assignment.launch`; it carries the exact selected configuration and its fingerprint, accepted goal, execution, criteria, authorized paths, declared write scopes, resource ceilings, and Worker Lease. Later stdin messages are typed steering or interruption controls.

The adapter must launch the configured Codex app-server or Claude Agent SDK session and emit JSON Lines on stdout. A conforming trace begins with `tyrion.adapter.ready`, including the native session ID, loaded native Skills, exact prepared Skill content identities, and the selected configuration fingerprint. Preparation alone is not invocation. Each actual invocation emits `tyrion.skill.invoked` with the exact content identity and inspected source; Tyrion requires every pinned launch default and permits only exact versions in the selected Worker capability inventory. The child does not attest containment. Tyrion injects containment evidence only after its own OpenShell preflight. The trace then carries native Codex app-server JSON-RPC notifications or normalized Claude Agent SDK session events, explicit usage, and exactly one terminal lifecycle state in valid order. Tyrion streams the native session, latest meaningful activity, and usage into live inspection while bounding total event output by `max_storage_bytes`. Native Codex interrupted turn completion and Claude `user.interrupt` semantics normalize to the same interrupted state. Terminal scraping does not satisfy this contract.

Reference production adapter sources live at `adapters/codex_app_server.py` and `adapters/claude_sdk_adapter.py`; both share `adapters/native_skill.py` for exact package identity and failure events. The Codex adapter uses the official app-server JSON-RPC interface, applies selected reasoning effort to both the thread configuration and turn, validates the effective thread model and effort, hashes the path returned by native Skill discovery, and sends every pinned default as an explicit typed Skill input with that native path. The Claude adapter leaves SDK setting sources at their native defaults, resolves personal Skills before project Skills from the working directory through the repository root, hashes each resolved package, exposes `skills="all"` and the native `Skill` tool, requests pinned defaults by native Skill name, and records only observed Skill tool-use blocks. A Claude Worker Configuration can map `settings.native_skill_paths` from names to absolute sandbox paths when native managed or plugin discovery does not expose source paths in the init event. Neither adapter puts Skill contents in the Assignment prompt or modifies the package. Because app-server exposes no hard monetary budget, this path fails closed unless the reserved model spend is zero and the provider is unmetered. The Claude adapter maps the exact reserved model-spend cents to the SDK budget, validates the effective model, forwards streaming clarifications, and uses the SDK interrupt API. A content mismatch or harness invocation failure emits `required_skill_failure`; Tyrion validates the report against an exact pinned launch default, retains the narrow failure observation, and reroutes only to an approximately equal eligible configuration that supports the same pinned defaults. Otherwise it opens an exact Attention Condition. Package either reference adapter with `native_skill.py` and its pinned native dependency as one executable artifact, then place that artifact's digest in the catalog. Catalog settings are fail-closed: each adapter rejects settings it cannot map to its native interface.

A completed Attempt also requires a typed `tyrion.result` record. The record is bound to the Commission, Assignment, Attempt, mandate revision, and plan revision from the launch message, includes a non-empty summary, reports exact model cost in cents, and reports `known_effects`. The current structured execution path accepts no external effects, so that array must be empty. A failed, malformed, mismatched, or incomplete trace fails closed.

Successful Results expose `skill_executions` with exact Skill Version, Worker Configuration, Assignment class, verification outcome, correction count, cost, latency, Principal intervention, provenance, and unchanged-native delegation. `skill_associations` retain the exact Commission/Assignment/Worker scope, linked verification or harness-report Evidence, confidence, and observation time. They are explicitly descriptive (`causal: false`, `global_ban: false`) and are not a separate Skill ranking or recommendation subsystem.

For `codex_git`, Tyrion creates the immutable base Git bundle and a fixed candidate-bundle destination before launch. The adapter must produce `refs/heads/tyrion-result` at that destination from the supplied base. Tyrion downloads the bundle before deleting the sandbox, then independently checks it for linear ancestry, authorized changed paths, storage ceiling, serialized Integration, and fresh contained candidate and assembled verification. The adapter never marks its own Result accepted.

The built-in contained Codex catalog entry is derived from the validated runtime configuration. Its visible configuration includes the real model, Codex version, OpenShell version, source revision, base image, CPU, memory, overlay storage, process ceiling, and a fingerprint of the complete pinned runtime file.

## Inspection and control

Each Attempt receives a durable Commission-local Worker Handle such as `Arya`. `commission inspect` exposes the handle, stable Worker ID, exact selected configuration, Assignment, routing rationale, start and Worker-execution elapsed time, latest meaningful activity, native session identity when supplied, usage, and available controls. Controls are the intersection of Active Attachment authority, selected configuration support, live delivery state, open recovery state, and remaining Attempt budget. A built-in Worker never advertises a control it cannot receive.

Only the Active Attachment may steer or interrupt, and it must have the negotiated `worker_steering` or `worker_interruption` capability. Both commands require the current Commission revision and an idempotency key. Tyrion commits a pending outbox record before delivery, includes the stable command ID in the adapter envelope, and atomically marks the record delivered or failed afterward. Steering uses a clarification-only envelope that marks the goal, criteria, Authority Envelope, and resource ceilings immutable. Interruption includes its journaled reason, revokes the active Worker Lease, preserves the Attempt as interrupted, and creates an actionable Attention Condition. `worker retry` explicitly resolves that condition and schedules a fresh routed Attempt when budget remains.

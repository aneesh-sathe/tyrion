# Pi Entry and Worker adapters

Pi has two independent production roles in Tyrion. The Entry adapter attaches Pi's native extension host to an existing durable Control Plane. The Worker adapter starts a fresh, contained Pi RPC process for one revision-bound Assignment. Running the Entry adapter never makes Pi eligible as a Worker.

## Entry Session

Start a normal Pi session through Tyrion:

```sh
tyrion --socket /path/to/tyrion.sock pi --pi-command pi -- --model provider/model
```

Tyrion issues a single-use launch token bound to the Pi adapter identity, writes the embedded extension through a user-owned `0700` cache beside the configured daemon socket, verifies the pinned digest through an anchored no-follow file descriptor, and restores the cached file to `0600` before launch. The socket parent and its resolved ancestor chain must not be group- or world-writable, so another user cannot replace the validated cache before Pi opens the extension. The launch credential exists only in the child environment, and the extension removes it before connecting. A direct Pi launch, a missing token, an incompatible protocol, or an incomplete capability manifest renders `Tyrion: Disconnected` and establishes no authority.

The default launch negotiates every supported Entry capability. Repeating `--capability NAME` supplies an explicit subset. `/tyrion-downgrade limited` removes material notifications; `/tyrion-downgrade observer` retains only inspection, replay, and the persistent mode display. Capability expansion is rejected for the life of that Attachment.

The extension exposes these native commands:

- `/tyrion-status`
- `/tyrion-propose JSON`
- `/tyrion-accept [COMMISSION_ID]`
- `/tyrion-replay [LAST_SEQUENCE]`
- `/tyrion-steer WORKER_HANDLE CLARIFICATION`
- `/tyrion-interrupt WORKER_HANDLE REASON`
- `/tyrion-retry WORKER_HANDLE`
- `/tyrion-propose-operation JSON`
- `/tyrion-gate APPROVAL_GATE_ID`
- `/tyrion-pause`, `/tyrion-resume`, and `/tyrion-cancel`
- `/tyrion-downgrade limited|observer`
- `/tyrion-takeover`

Approval remains an independent Principal action. Pi renders the canonical gate, consequences, limits, and a decision-first warning, but the Entry adapter cannot approve its own proposed operation. Material events are replayed automatically from the last durable cursor and are also available through explicit replay. Reconnecting with `--commission-id` and `--last-event-sequence` replays work completed while Pi was absent.

Every visible Commission summary includes the Attachment role and capability effects, Authority Envelope, resource ceilings, Acceptance Criteria, Worker handles and available controls, selected configurations, routing rationale, activity and usage, Results, Evidence, verification decision, and completion briefing. Automatic and explicit replay advance one monotonic cursor, so an older in-flight replay cannot overwrite newer command state or redeliver material events.

## Worker eligibility

A catalog entry with `adapter.kind: "pi_rpc"` is a production Worker only when it declares `settings.production_qualified: true`, is available, supplies the complete shared capability set, and passes the same digest, authority, Skill, context, resource, routing, restart, and containment checks as other structured adapters. An incomplete entry remains in the catalog but appears under `route.rationale.ineligible` with the `production_qualification` gate. Tyrion never substitutes a simulated Pi Worker.

The runtime JSON needs a pinned `pi` block using `runtime/openshell/hard-landlock-pi-policy.yaml`, `model_provider: "openai"`, and the exact `openai/MODEL_ID` accepted by qualified catalog entries. The provider-specific policy permits `/sandbox/pi` to contact only the OpenAI API endpoint. Pi runs with `--mode rpc`, ephemeral sessions, no discovered extensions, context files, prompt templates, or Skills. The selected model, built-in tool allowlist, native session ID, exact Skill command inventory and native path, Skill package digest, structured lifecycle, authoritative session-total usage, zero spend, semantic steering and interruption, typed Result, and terminal state are all validated. Interruption clears queued steering and follow-up messages before aborting. `max_model_spend_cents` must be zero because Pi RPC exposes no hard monetary budget control, and final native session statistics must report exactly zero cost.

Selected native Skills use `settings.native_skill_paths` to name `SKILL.md` files already present inside the accepted sandbox workspace. Relative paths resolve inside the cloned Assignment repository and cannot escape it; absolute paths must already be reachable inside the sandbox. The adapter disables discovery, loads only that path explicitly, verifies the native command's source, canonical path, and content identity, and invokes the single pinned Assignment Skill with Pi's native `/skill:name` command. Assignments selecting more than one distinct Skill are visibly ineligible at routing with `pi_single_native_skill`; they are never dispatched into a known adapter failure. Git Assignments receive only Tyrion-created bundle bindings and return a candidate bundle for independent validation and Integration.

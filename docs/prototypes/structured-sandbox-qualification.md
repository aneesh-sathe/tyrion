# Structured Docker Sandbox qualification

Date: 2026-09-11. Tracking: [issue #17](https://github.com/aneesh-sathe/tyrion/issues/17).

## Decision

**Blocked before real Worker launch. Production integration is not qualified.**
No model was invoked, credential used, repository uploaded, or sandbox created
during this follow-up. Issue #17 remains open; this record does not satisfy its
real-execution acceptance criteria or unblock #18.

## Evidence inspected

- Installed `sbx version`: `v0.42.1`, revision
  `cc6e400a4a3ce3ce5e0b2b77b8ee352aac854c64`.
- Installed `sbx create --help` documents mountless creation by omitting the
  workspace argument and explicit CPU/memory sizing. Its `--ttl` control is
  cloud-only. Help does not establish effective isolation or enforcement.
- Installed `sbx exec --help` supports `-i` separately from `-t`, providing a
  candidate non-TTY transport for the existing adapter. It also states that
  exec starts a stopped sandbox: never use it to prove post-stop cleanup.
- `adapters/codex_app_server.py` launches `codex app-server --strict-config`,
  reads `tyrion.assignment.launch`, and accepts only zero model-spend and
  paid-service-spend reservations. Its explicit error requires an unmetered
  provider because app-server has no hard monetary budget control. A zero
  reservation alone does not prove that the selected credential is unmetered.
- The earlier local no-model report recorded 270 live children against the
  256-process requirement, filesystems larger than the existing 4 GiB profile,
  and only a short image ID. Those are prior observations, not rerun results.
  Their original evidence remains in `.scratch/sbx-qualification-20260911/`.

## Launch blockers

1. The Principal has not specified the authorized provider/model, credential
   source, disclosure scope, and spend limit for this run. Docker login and
   authorization to implement the ticket do not grant that model-use authority.
   A metered configuration additionally needs a proven external hard spend
   control; changing the adapter's guard would not supply one.
2. The tested runtime has not demonstrated the existing process and total
   writable-storage limits. Preserve those requirements until stock controls
   demonstrate them or the Principal records a specific alternative contract.
3. A complete immutable image identity and repeatable resolution are still
   missing. A registry digest alone must not be mistaken for evidence that the
   runtime actually launched that image.
4. Local lifetime enforcement and cleanup after execution-client loss need an
   independently observed mechanism. The cloud-only TTL flag is not evidence
   of a local watchdog.

## Bounded continuation

Use a new synthetic Git repository containing only `result.txt` with
`before\n`; request exactly `verified sandbox change\n`. Pin the full input
commit and bundle digest. Proposed disclosure is that synthetic input, the
Assignment, and the existing Codex adapter plus `native_skill.py`. No personal
project source or host authentication directory is included.

Before model use, resolve the blockers and inspect the actual mountless Worker
boundary for checkout/control-state/sibling isolation, disabled shared Skills
and SSH forwarding, absent host MCP tools and ambient service credentials, and
provider-only approved egress. Distinguish credential placeholders from secrets
without printing credential values. Fail closed on any unresolved required
property. Record CPU, memory, total writable disks, nested Docker storage,
caches, logs, verifier overlap, lifetime, process behavior, and spend controls
separately from the Result artifact budget.

Then transfer the checked Git bundle and existing adapter into a uniquely named
disposable Worker. Send a revision-bound Assignment over `sbx exec -i` without
a TTY; preserve redacted lifecycle, usage, and terminal events. Record harness,
adapter, model/settings, runtime/image identity, input revision, and candidate
bundle digest. Validate returned Git objects and authorized paths before using
the candidate. In a separate fresh sandbox, verify the exact requested content
and absence of Worker environment mutations. Do not run project tests on the
host as candidate verification.

Exercise bounded interruption and execution-client loss within the approved
run budget. Retain each sandbox identity before dispatch, observe shutdown
without restarting it, and remove only resources created by this experiment.
Report completion, interruption, and client-loss cleanup independently. Preserve
redacted evidence and required artifacts after cleanup.

## Qualification status

| Claim | Status |
| --- | --- |
| Local runtime version and documented CLI transport | Inspected; not an execution qualification |
| Existing 256-process limit | Failed in prior bounded probe; not rerun |
| Existing 4 GiB runtime profile | Not met by prior configuration; not rerun |
| Immutable image identity and repeat resolution | Untested |
| Real structured lifecycle, usage, candidate and fresh verification | Untested |
| Effective model credential, egress and spend boundary | Untested |
| Structured interruption and client-loss cleanup | Untested |
| Changed resource contract | Proposed decision only; no change authorized |

The smallest next Principal input is an explicit provider/model, credential
source, synthetic disclosure authorization, and spend limit. That input does
not resolve the independent containment and resource blockers.

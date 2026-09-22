# Docker Sandboxes evaluation and integration plan

Status: breakdown approved and issues published as #17, #18, and #19.
Parent: https://github.com/aneesh-sathe/tyrion/issues/16

## Objective

Determine whether stock Docker Sandboxes can execute Tyrion's real coding work with a small amount of integration code. Adopt it only after evidence supports the required boundary. Preserve the current OpenShell implementation while evaluating the candidate; do not commit to maintaining multiple backends.

The no-model qualification passed sampled isolation, bundle transfer, fresh verification, and cleanup checks. It also demonstrated that the tested configuration does not enforce 256 processes and uses larger disks than the existing profile. Immutable image identity, model authentication, structured harness operation, and production recovery remain unqualified. See [the recorded experiment](../prototypes/docker-sandboxes-qualification.md).

## Scope and decision rules

- Start with a real structured Worker Attempt outside production dispatch. A shell smoke or working TUI cannot substitute for it.
- Keep the daemon, SQLite, approval authority, integration repository, and effect broker outside Worker VMs. Use mountless workspaces, isolated Skill inputs, explicit egress and credential grants, and fresh verification.
- Make no automatic withdrawals of security requirements. Report each required property as demonstrated, failed, or untested. Any narrower resource contract needs an explicit Principal decision before production eligibility changes.
- Distinguish VM/runtime disk consumption from the accepted Result artifact budget. Account for writable disks, nested Docker storage, logs, caches, and simultaneous Worker/verifier VMs. Do not reinterpret the Commission's existing storage ceiling to make a default image fit.
- Treat model-spend enforcement separately from credential secrecy and reported usage. Select a provider configuration and spend limit explicitly; ambient credentials and prior Docker login grant no model-use authority.
- Prefer stock supported controls. If an essential property requires a custom kernel, custom proxy, undocumented state manipulation, or a broad runtime framework, return an actionable Blocker and reconsider the candidate.
- No standalone prefactoring issue is warranted by the inspected code. If a tiny lifecycle seam is needed, extract it at the beginning of the integration slice with unchanged behavior and existing tests passing. Do not create a generic backend registry, scheduler, installer service, or policy language.

## Published slices

| Draft | Title | Blocked by | Independently verifiable outcome |
| --- | --- | --- | --- |
| [#17](https://github.com/aneesh-sathe/tyrion/issues/17) | Qualify a real structured Worker Attempt in Docker Sandboxes | None | Real adapter input, streamed output, Git candidate, fresh verification, bounded interruption, and a go/no-go record |
| [#18](https://github.com/aneesh-sathe/tyrion/issues/18) | Complete and recover a single Commission in Docker Sandboxes | #17, with a positive qualification decision | An attached Entry Session drives one Commission through acceptance, execution, verification, export, and demonstrated safe restart recovery |
| [#19](https://github.com/aneesh-sathe/tyrion/issues/19) | Integrate cross-harness Assignments in Docker Sandboxes | #18 | Codex and Claude execute useful disjoint work concurrently and produce one verified integrated artifact under Commission-wide limits |

These are derived stories from the conversation, not additional promises added to the parent specification:

- A: As the Principal, I can judge a real Worker on the stock runtime before paying the complexity cost of migration.
- B: As the Principal, I can delegate one bounded task and receive a verified artifact or actionable Blocker without losing authority or work across interruption/restart.
- C: As the Principal, I can obtain one assembled Result from useful cross-harness work while observing and controlling it through one Entry Session.

Full published issue bodies are retained in the project's `.scratch/sbx-issues/` directory. All three issues carry `ready-for-agent`, real blocker references, and native GitHub dependencies (#18 blocked by #17; #19 blocked by #18). The label indicates an implementation-ready description; an unresolved dependency still blocks execution.

## Return to the existing dogfood ticket

After C, use existing issue 16 rather than creating a duplicate pilot issue. Its single real Commission must combine concurrency, current verification, a controlled approved effect, planned recovery, advisory preference application, and a complete exported record. A simple explicitly approved disposable local-file effect can exercise the existing broker without granting a general Worker GitHub or cloud credentials.

Do not modify or close either parent issue while publishing these slices. Qualification reports and individual successful Commissions do not satisfy the combined dogfood criterion.

OpenShell retirement is conditional follow-up after the pilot, not a commitment made by this plan. Before removal, inventory all remaining consumers, including Pi Workers, verification, and exceptional credentialed Effect Sandboxes. Each must migrate, be explicitly deferred, or remain a documented blocker to removal. Do not expand this initial evaluation into a speculative full-platform migration.

## Approval record

The Principal approved the three-slice breakdown and dependency chain before publication. Approval covers this plan and tracker publication; model-use authorization and any resource-contract change remain subject to the explicit requirements in #17.

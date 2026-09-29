# Results

Every number in the README comes from a run with real models, in the real
container boundary, on one Apple Silicon Mac. The exported records stay
private because they carry machine-specific paths. Run your own and export the
record with `tyrion commission export-record`.

| Date | What happened |
| --- | --- |
| 2026-09-29 | **Twenty-one agents at once, the most this Mac admits.** All 21 verified first time: 956 seconds of agent work finished in a 58-second window, 16.6 times faster than one at a time, and 85 seconds from start to verified. Each agent stayed under 300 MiB of memory. The first attempt found a real limit: Docker ran out of network addresses at the sixteenth agent, so Tyrion now assigns every agent's networks itself |
| 2026-09-28 | **Every guarantee in one job.** Codex and OpenCode agents extended a Python project in parallel, and the dependent piece waited for the two it needed. During the job, an approved file write happened outside the checkout, 78 attacks were run inside the three live agent containers (none got through), one agent was interrupted and retried, and a lost session was replaced without losing any work. A learned preference reached every agent, and every agent followed it. Verified complete in 109 seconds, and reproduced twice more |
| 2026-09-28 | **Surviving a crash.** The daemon was killed mid-job with two agents running. On restart it removed their containers, retried both pieces, and finished verified in 57 seconds |
| 2026-09-27 | **OpenCode and Codex together.** Four agents on two harnesses finished in 63 seconds, against 200 seconds one at a time |
| 2026-09-27 | **Tyrion planned the job itself.** A planning agent split a one-line goal into four pieces that ran in parallel and passed first time, 162 seconds faster than one at a time |
| 2026-09-27 | **One sentence, typed into Codex.** It became a four-part plan, verified first time, 137 seconds faster than one at a time. The same sentence typed into Claude Code finished 96 seconds faster |
| 2026-09-26 | **Ten agents at once on one Mac.** All ten verified, seven times faster than one at a time, using 1.8 GiB of memory together |
| 2026-09-24 | **Codex and Claude together.** Two harnesses on separate work produced one verified result, 20 seconds faster |
| 2026-09-21 | **The container, measured.** With no agents involved, every limit held, and root inside the container could not raise any of them |

## How far it scales

Tyrion admits an agent only while the machine can hold it. Each agent reserves
a quarter of a CPU core and 320 MiB of memory, with 1 GiB kept back for Docker
itself. That gives these ceilings, before any limit your model provider sets:

| Docker has | Agents at once |
| --- | --- |
| 12 cores, 8 GB (measured above) | 21 |
| 12 cores, 16 GB | 48 |
| 16 cores, 32 GB | 64 |
| 32 cores, 64 GB | 128 |

## What each record holds

An exported record is one JSON file with a SHA-256 checksum over its contents:

- what you approved: the goal, the checks, the authority and the limits
- the plan, where each piece went and why
- every result, every check, and the order the work was merged
- every approval, recovery and restart
- a report that keeps approvals, planned and unplanned interventions,
  corrections, timing and cost apart

The record never certifies itself: its `readiness` is `blocked` when something
is wrong, and otherwise `unassessed`. The judgement stays with you.

# Capacity

How many Workers Tyrion runs at once on one machine, and why the numbers are
what they are. Decided 2026-09-26 from measurement, and revised 2026-09-27
after the harnesses moved into the Worker image.

## The decision

Every Worker has two sets of numbers.

| | CPU | Memory | Files | Processes |
| --- | --- | --- | --- | --- |
| **Ceiling**, enforced by the container runtime | 2 vCPUs | 3072 MiB | 2048 MiB | 256 |
| **Request**, reserved by admission | 0.25 CPU | 320 MiB | | |

Admission sums the requests of every running Worker, across all Commissions,
against what the container runtime reports, less 1024 MiB kept back for the
engine and the egress relays. The ceilings contain a Worker that runs away.

On a 12-CPU Docker VM with 7.7 GiB that admits **21 Workers at once**, and a
real run of 21 concurrent Codex Workers verified all 21 (2026-09-29, 16.6 times
faster than serial). With 16 GiB, memory and CPU both bind at 48.

The previous profile was 2 vCPUs and 6144 MiB, reserved in full. It was
inherited from an earlier sandbox design and never measured. On the same
machine it admitted one Worker, and every container was pinned to the same
two CPUs.

## What was measured

Real Codex Workers (`codex-cli 0.156.1`, `gpt-5.6-sol`, low reasoning effort)
in the `docker-hardened-v1` container, each implementing a small feature with
unit tests in a Python project: reading the code, writing two files, and
running the test suite. A sampler read each container's own cgroup once a
second.

| Run | Workers at once | Outcome | Per-Worker peak | All Workers peak | CPU per Worker |
| --- | --- | --- | --- | --- | --- |
| Serial | 1 | measured | 576-604 MiB | | 0.05 avg, 0.32 max |
| Concurrent | 6 | 6/6 accepted | 568-582 MiB | 3233 MiB | 0.04 avg, 0.17 max |
| Concurrent | 10 | 10/10 accepted | 537-570 MiB | 5127 MiB | 0.04 avg, 0.14 max |
| Default admission | 10 | 10/10 accepted | 554-581 MiB | 5144 MiB | 0.04 avg, 0.11 max |
| **Harnesses in the image** | **10** | **10/10 accepted, 0 failed Evidence** | **255-293 MiB** | **1794 MiB** | **0.02 avg, 0.10 max** |

The first four runs streamed the Codex and code-mode-host binaries into each
Worker's tmpfs: 328 MiB of memory-charged files per Worker, identical in every
one. The default-admission run used no capacity override (the daemon admitted
all ten from Docker's own figures) and finished 565 seconds of Worker execution
in a 64-second window, 8.8 times faster than serial.

The last run built the harnesses into the read-only image instead, so every
container shares one copy. A Worker's peak now splits into roughly:

- 150-190 MiB of process memory
- 32 MiB of files in its `/sandbox` tmpfs: the repository and its work
- about 35 MiB of reclaimable page cache

That halved each Worker, cut ten Workers from 5.1 GiB to 1.8 GiB, and let the
request fall from 640 MiB to 320 MiB. 454 seconds of Worker execution finished
in a 65-second window, 7 times faster than serial.

Per-Worker memory did not grow with concurrency, so there was no hidden
contention cost. CPU was never the constraint: a Worker spends its time
waiting on the model.

## Why requests and ceilings, not one number

Reserving the ceiling wastes the machine: ten Workers that fit comfortably in
5 GiB were refused because each might have used 6. Shrinking the ceiling to
the measured size would admit them but kill any Worker that runs a real build,
because `npm install` or a large test suite needs far more than a small Python
change.

Separating the two is the standard answer. The request is what a Worker is
expected to use, taken from the measured 293 MiB peak with headroom. The
ceiling keeps one runaway to 3 GiB, under 40% of this machine, so it cannot
starve the rest.

The risk is that several Workers exceed their requests at once. The container
runtime then kills a container, not the host: Tyrion's daemon runs outside the
VM. The Attempt fails, and recovery treats it like any other failure.

## What this does not cover

- **Claude Workers are unmeasured.** `tyrion init` reserves 1024 MiB for them
  until they are. Measure them the same way before lowering it.
- **Heavier work is unmeasured.** These were small Python changes. A Worker
  building a large project will use more of its ceiling. Declare a heavier
  `containment_resources` request on that Worker Configuration.
- **Model rate limits.** Twenty-one concurrent Codex Workers on one ChatGPT
  subscription hit no limit here; a larger factory might.

## Reproducing

While a Commission runs, read each container labelled `tyrion.attempt` once a
second with `docker exec`: `memory.peak`, `memory.current`, the `anon`, `shmem`
and `file` lines of `memory.stat`, and `usage_usec` from `cpu.stat`, all under
`/sys/fs/cgroup`. `memory.peak` is a high-water mark, so sampling once a second
misses nothing but the last second of each container.

# Docker containment qualification

Date: 2026-09-21. Host: Apple silicon, macOS 26.6.2, Docker Desktop 28.0.4
(`overlayfs` snapshotter, guest kernel 6.10.14-linuxkit). Scope: local,
deterministic, no coding agents and no model calls.

## Verdict

**Plain Docker qualifies as Tyrion's Worker containment boundary on macOS.**
Every ceiling the existing profile requires is enforced by the Docker daemon
from outside the container and is not raisable by guest root, including the
256-process ceiling that stock Docker Sandboxes could not enforce. No custom
kernel, no patched init, and no Tyrion-distributed virtualization artifact is
involved: the operator provisions one digest-pinned image and Tyrion pins the
CLI, the daemon address, and the image identity.

Two mechanism findings changed the implementation and are load-bearing:

1. **`docker cp` silently no-ops into a tmpfs mount.** It exits 0 and reports
   nothing while writing underneath the mount instead of into it. All transfer
   uses `docker exec -i` streaming instead, which round-trips byte-identically.
2. **Docker Desktop leaves seccomp unconfined by default.** `Seccomp: 0` in a
   default container. `--security-opt seccomp=builtin` is mandatory and yields
   `Seccomp: 2`.

## Why not the alternatives

| Option | Outcome |
| --- | --- |
| Repaired OpenShell MicroVM | Works, but requires Tyrion to rebuild and distribute a Linux 6.12.76 libkrunfw with Landlock in `CONFIG_LSM` plus a patched guest init. That distribution burden is the blocker this qualification removes. |
| Docker Sandboxes (`sbx`) | Rejected. No process-ceiling flag and no storage flag exist at all; `--allow-network` and `--ttl` are cloud-only, so deny-by-default plus provider-only egress is not expressible locally; `--skills` defaults to a writable mount and `--clone` bind-mounts the host repository and wires a git remote back to it. The local flag surface moved twice in ten days (0.42.1 to 0.45.0). It is a developer-convenience product, not a containment substrate. |
| Apple `container` | Deferred. VM-per-container is architecturally the right successor, but it is young, thin on resource flags, and churning. Revisit in a year; the seam makes that cheap. |

## Observed results

Probe scripts: `.scratch/docker-qual-20260921/probe.sh`, `probe2.sh`,
`probe3.sh`. Image under test: a locally present `rust:1.95-bookworm`
(`sha256:6258907abe69...`), chosen because it carries `git`, `curl`, and
`python3` like the real Worker image.

| Check | Result | Evidence |
| --- | --- | --- |
| 256-process ceiling | **Passed** | `--pids-limit 256`: `fork` failed with `EAGAIN` at child 255; `pids.max=256`, `pids.current=256`. Docker Sandboxes ran 270. |
| Guest root cannot raise ceilings | Passed | `/sys/fs/cgroup` mounted `ro`; writing `pids.max` returned `Read-only file system`. |
| CPU ceiling | Passed | `cpu.max = 200000 100000`. With `--cpuset-cpus 0-1`, `nproc = 2`. |
| Memory ceiling | Passed | `memory.max = 2147483648`, `memory.swap.max = 0`; `tail /dev/zero` under `--memory 512m` was OOM-killed (exit 137). |
| Writable storage ceiling | Passed | `--read-only` rootfs plus a sized `/sandbox` tmpfs: `df` reported exactly the requested size and a 400 MiB write into a 256 MiB tmpfs stopped at 256 MiB. |
| Read-only root filesystem | Passed | `/etc` write returned `EROFS`; `/sandbox` writable. |
| Privilege reduction | Passed | `--cap-drop ALL --security-opt no-new-privileges --security-opt seccomp=builtin --user 65534`: `CapEff=0000000000000000`, `NoNewPrivs=1`, `Seccomp=2`, `uid=65534`. |
| Default seccomp | **Finding** | Without the explicit option, `Seccomp: 0`. Docker Desktop reports `name=seccomp,profile=unconfined`. |
| No host filesystem | Passed | Principal checkout, its parent, the daemon state directory, `/var/run/docker.sock`, `~/.ssh`, and `~/.aws` all absent. `/proc/self/mountinfo` contained only the container's own overlay root, `/proc`, `/sys`, `/dev`, `/sandbox`, and the three generated `/etc` files. |
| No ambient credentials | Passed | Every provider, cloud, and SSH variable empty in a container created with no `--env` for them. |
| Egress denied | Passed | `--network none`: no interface but `lo`, DNS fails, direct IP connection fails. |
| Internal network denied | Passed | `docker network create --internal`: direct IP blocked, DNS blocked. |
| Brokered egress | Passed | Internal bridge plus a destination-pinned TCP relay, reached by `--add-host <host>:<relay ip>`: the allowed destination returned HTTP 200 over end-to-end TLS while a direct IP and any other hostname stayed blocked. |
| Sibling isolation | Passed | Neither container saw the other's canary or process table. |
| Transfer round trip | Passed | Git bundle in over `docker exec -i`, committed candidate out, then host-side `git bundle verify`, bare quarantine clone, `git fsck --strict`, and changed-path inspection all passed with exactly `result.txt` changed. |
| Fresh verifier isolation | Passed | A third container read the candidate and carried none of the Worker's state. |
| Forced removal | Passed | `docker rm -f` removed the container and its background process; absence confirmed by `docker inspect` failing, never by `docker exec`, which restarts a stopped container. |
| Launched image identity | Passed | `docker inspect --format '{{.Image}}'` on the created container returns the exact image ID that launched. This closes the identity gap Docker Sandboxes left open. |

`curl` without `--fail` exits 0 on HTTP 403, so every egress probe recorded the
HTTP status rather than the exit code.

## The resource contract changed, and needs a Principal decision

The OpenShell profile was 2 vCPU, 2048 MiB memory, and a 4096 MiB overlay disk:
6 GiB of host resources with two independent ceilings.

Docker charges tmpfs pages to the container memory cgroup, so a single memory
cgroup can bound process memory and writable files together. `--storage-opt
size=` is unsupported on Docker Desktop's `overlayfs` driver, and a polled
disk-usage watchdog would not be hard enforcement. The implemented profile is
therefore **2 vCPU, 6144 MiB combined memory-and-files, 4096 MiB writable
storage, 256 processes** - the same 6 GiB host envelope, expressed as one hard
combined ceiling plus a hard storage sub-ceiling.

This is a strictly better-enforced statement than the previous one, but it is
still a contract change. It is implemented and pinned in code; production
eligibility should not flip until the Principal records the decision.

## What the boundary claims, and what it does not

1. Host macOS, the Principal checkout, `tyriond` state, and host credentials
   are unreachable from a Worker absent a hypervisor escape. *Mechanism: Apple
   Virtualization.framework, maintained by Docker.* **Residual risk:** Docker
   Desktop's VM holds a virtiofs channel to the default host file-sharing list
   (`/Users` among them), so a VM escape on Desktop is worse than on a generic
   Linux VM. A Colima or Lima context with no host sharing removes it.
2. A Worker and its descendants are bounded at 256 processes. *Daemon-set
   `pids.max` over a read-only cgroupfs.*
3. Memory and writable files are bounded together at 6144 MiB, and files alone
   at 4096 MiB. *Memory cgroup plus tmpfs sizing.*
4. CPU is bounded at a 2-core CFS quota, with the cpuset matching. *Not a
   guarantee about scheduling latency.*
5. No host path reaches a Worker; transfer is verified Git bundles streamed
   through `docker exec`. *No bind mount exists at create time; preflight
   asserts it from inside.*
6. Egress is deny-by-default with exactly one brokered hop per authorized
   destination. *Internal bridge plus destination-pinned relay.*
7. The image that launched is identified by sha256. *Post-create `docker
   inspect`; Tyrion never pulls.*
8. **Sibling Attempts are isolated at namespace strength, not VM strength.** A
   Linux kernel exploit crosses between Workers and compromises the VM, though
   not macOS. This is a real reduction from the MicroVM backend and is stated
   rather than papered over.
9. Deletion is confirmed by `docker rm -f` plus an independent absence check.

Not claimed: VM-strength sibling isolation, model-spend enforcement, or that
hiding a credential bounds its authority. Provider access conveys spend and
disclosure authority regardless of containment, so Tyrion's effect gates and
Commission ceilings remain the controls for that.

## Still unqualified

Real structured harness I/O, model authentication, spend enforcement, native
Skill preparation, interruption, daemon-crash recovery, raw UDP and ICMP,
redirect and destination-confusion attacks, symlink and hard-link transfer
attacks, and root-level VM escape were not tested. Issue #17 remains the next
gate and still needs an explicit Principal decision on provider, model,
credential source, and spend limit.

## Host changes made

Docker Desktop, already installed, was started. No setting was changed, no
image was pushed, and every container, network, and image created by these
probes was removed. `sbx` remains installed from the 2026-09-11 experiment and
is now unused by Tyrion. Pulls initially hung for several minutes while Docker
Desktop finished cold-starting; that was not a configuration fault.

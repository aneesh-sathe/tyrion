# Sandbox reassessment for issue 16

Status: **resolved on 2026-09-21.** Plain Docker replaced the repaired
OpenShell MicroVM as the Worker containment boundary. The decision record and
complete claim set are in
[`prototypes/docker-containment-qualification.md`](prototypes/docker-containment-qualification.md);
this file retains the reasoning that led there.

## How the question resolved

The original problem was never containment strength. It was that Tyrion had to
rebuild and distribute a Linux 6.12.76 libkrunfw with Landlock in
`CONFIG_LSM`, plus a patched guest init for a 256-process cgroup, and then keep
doing so. That is a maintenance obligation a personal runtime cannot carry.

Three candidates were considered.

**Docker Sandboxes (`sbx`) was rejected.** The 2026-09-11 no-model experiment
already found it could not enforce 256 processes (270 children ran) and used
disks far larger than the 4 GiB profile. Re-inspecting the installed CLI showed
why: **no process-ceiling flag and no storage flag exist at all**, so that was
structural rather than misuse. `--allow-network` and `--ttl` are cloud-only, so
deny-by-default plus provider-only egress cannot be expressed locally at all.
`--skills` defaults to a writable mount and `--clone` bind-mounts the host
repository and wires a git remote back to it. The local flag surface moved from
0.42.1 to 0.45.0 in ten days. It is a developer-convenience product, not a
containment substrate.

**Apple `container` was deferred.** VM-per-container is architecturally the
right successor, but it is young, thin on resource flags, and churning fast.
Adopting it now would trade a maintained custom kernel for a moving young
runtime, which is the same trap. The seam makes revisiting it cheap.

**Plain Docker qualified.** The argument that unlocked it: *on macOS the kernel
a container shares is already a disposable Linux VM kernel under Apple's
Virtualization.framework, not the host macOS kernel.* Host protection therefore
rests on a hypervisor boundary that Docker maintains, and the "containers share
the host kernel" objection does not apply to a MacBook host. Everything Tyrion
needs above that is a stock daemon-enforced flag: `--pids-limit`, `--memory`,
`--cpus`, `--cpuset-cpus`, `--read-only`, a sized tmpfs, `--cap-drop ALL`,
`--security-opt no-new-privileges`, `--security-opt seccomp=builtin`,
`--user`, and `--network none` or an `--internal` bridge. Every one was
measured, including the 256-process ceiling that `sbx` could not enforce.

The cost is one honest reduction: sibling Attempts are now isolated at
namespace strength rather than VM strength. That is stated in the claim set
rather than papered over.

## Assumptions that were challenged, and how they landed

1. **Tyrion must distribute a custom kernel.** False. Pinning a supplied
   runtime and a digest-pinned image delegates virtualization maintenance to
   its supplier, which was the point of the reassessment.
2. **Writing guest `/etc` means escaping the sandbox.** The old probe did
   misread this. Under `--read-only` the root filesystem is genuinely
   immutable, so the check is now a true statement about the profile rather
   than a proxy for escape.
3. **Every Docker socket is forbidden.** Still true for the *host* runtime
   socket, and the preflight asserts its absence. A daemon wholly inside a
   disposable VM would have been a different question; it did not arise.
4. **Hiding a credential also limits its use.** Correct, and it still holds.
   Destination pinning prevents sending a credential elsewhere, but provider
   access conveys spend and disclosure authority regardless of containment.
   Tyrion's effect gates and Commission ceilings remain the controls.
5. **All-or-nothing dogfood is the next experiment.** Correct. This was the
   no-model boundary stage. Issue #17 (one real Worker) is next and still needs
   a Principal decision on provider, model, credential source, and spend limit.

## Contract changes this decision carries

- **Resources.** 2 vCPU / 2048 MiB memory / 4096 MiB overlay became 2 vCPU /
  6144 MiB combined memory-and-files / 4096 MiB writable storage / 256
  processes. The memory cgroup charges tmpfs pages, so one hard ceiling now
  covers process memory and files together, with a hard storage sub-ceiling
  beneath it. Same 6 GiB host envelope, better enforced. `--storage-opt size=`
  does not work on Docker Desktop's `overlayfs` driver and a polled watchdog
  would not be hard enforcement, so this framing is the honest one.
- **Credentials.** The OpenShell provider kept the credential out of the
  Attempt entirely and substituted it in the proxy. Docker has no equivalent,
  so a declared credential now reaches the Worker execution's environment,
  scoped to one `docker exec` and never to the container, a command line, or
  durable state. Building a TLS-terminating header-injecting proxy was
  considered and rejected as bloat for a benefit that does not bound authority.

Both are implemented and pinned in code. Production eligibility should not flip
until the Principal records these two decisions.

## Remaining OpenShell consumer

The credentialed one-shot Effect Sandbox in `src/credential.rs`, started by
`tyriond --credential-runtime <json>`, is the last consumer. It uses the same
small create/transfer/execute/delete seam and is the remaining migration before
`runtime/openshell/` can be deleted. It is an exceptional path used only when
the Principal approves a one-shot credentialed effect, not a per-Attempt one.

## Implementation boundary, as predicted

The replacement stayed inside the `Sandbox` seam in
`src/worker/contained_codex.rs`: create, transfer, execute, delete. Git
validation, Integration, leases, recovery fences, and the adapter protocols are
untouched. No generic backend registry, scheduler, custom proxy, policy
language, installer service, or second database was added. The containment
profile is named `docker-hardened-v1` rather than reusing the OpenShell
identity, so no evidence is mislabelled.

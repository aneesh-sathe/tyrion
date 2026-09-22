# Docker Sandboxes qualification

Date: 2026-09-11. Scope: local, deterministic, no coding agents or model calls.

## Verdict

Stock Docker Sandboxes is a promising replacement for the repaired OpenShell distribution, but it does **not** qualify for Tyrion's existing containment profile unchanged. Basic mountless isolation, bundle transfer, fresh verification, and explicit VM cleanup worked on the dogfood host. A fixed-size process probe exceeded the current 256-process requirement. Default storage also exceeds the current 4 GiB profile.

Recommendation: retain the stock runtime and decide whether the MVP should promise bounded VM CPU, memory, disk, and lifetime rather than an exact guest process count. If that narrower resource contract is acceptable, continue with a small structured Worker integration. Do not patch another kernel to preserve an arbitrary numeric default. This is a proposed contract revision, not an implemented relaxation. Issue 16 remains open.

## Installed runtime and host changes

- Installed `docker/tap/sbx` through Homebrew, with unrelated auto-update and cleanup disabled.
- Version: `v0.42.1`, revision `cc6e400a4a3ce3ce5e0b2b77b8ee352aac854c64`.
- Installed binary SHA-256: `9e4463abbbda9f668898565c196decaa086f4472987082968f92a4330de4f344`.
- Host: Apple silicon, macOS `26.6.2`; observed guest kernel: `7.0.12`.
- Docker account login completed interactively by the Principal. No provider credentials were imported and no model execution was attempted.
- There were no existing sandboxes or registered MCP servers. Service-secret inspection returned zero stored secrets, zero custom secrets, and zero environment-only secrets; values were not printed.
- Initialized the previously uninitialized global network policy to `deny-all`.
- Changed `ssh.agentForwardingEnabled` from its default `true` to `false`, then restarted the new daemon before creating sandboxes.
- These two settings remain in effect. Docker Sandboxes, its daemon, login state, and downloaded image cache remain installed. No existing Docker Engine configuration was changed.

The installed CLI accepts the hidden `--no-share-skills` flag even though its generated create help does not list it. Runtime metadata confirmed `ShareSkills: false`, empty workspace and SSH socket paths, and no additional workspaces.

The shell image was `docker.io/docker/sandbox-templates:shell-docker`. `sbx template ls --json` reported image ID `5fc81bc7a127` and size `588725040` bytes. The initial pull was substantial; later creates reused downloaded layers. The local listing exposes only a short image ID, and `template inspect` is cloud-only in this release. A complete immutable image digest was **not** captured; do not treat this as a fully pinned production run.

## Observed results

| Check | Result | Evidence and limit |
| --- | --- | --- |
| Mountless workspace | Passed sampled checks | Host checkout and synthetic project canary absent from guest. Runtime mount list had no project or skills mount. |
| Generated host files | Read-only mounts | `/etc/hosts` and `/etc/resolv.conf` were runtime-generated bind mounts marked `ro`; mountless does not literally mean zero host-backed files. |
| Sibling isolation | Passed canary checks | Worker A's canary and guest `/etc` mutation absent from B; B retained its own canary after A stopped. Not an exhaustive escape test. |
| Fresh verifier | Passed | Third VM checked candidate contents and lacked Worker A's guest modification and both Worker canaries. |
| Artifact round trip | Passed | Input Git bundle copied in; committed candidate copied out; host `git bundle verify`, bare quarantine clone, `git fsck --strict`, and changed-path inspection passed. Only `result.txt` changed. |
| SSH forwarding | Disabled and unusable | Environment variable remained, but socket did not exist; `ssh-add -l` exited 2. Variable presence alone would have been a false positive. |
| Credential placeholders | Partially classified | OpenAI and Anthropic variables matched known placeholder strings. A GitHub variable also existed but was not classified by the limited matcher. No service secrets were available in the host store. This is not a comprehensive secret-leak attestation. |
| Denied HTTPS/DNS | Passed sampled checks | Proxied `https://example.com` returned HTTP 403; DNS lookup failed; proxy-bypassed hostname request failed DNS. Direct `https://1.1.1.1` terminated during TLS with no HTTP response. |
| CPU/memory sizing | Observed as requested | Guest reported 2 CPUs and 2,066,016 KiB total memory. This was sizing inspection, not an OOM or CPU stress test. |
| 256-process ceiling | Failed | Both relevant guest `pids.max` values were `max`; a bounded script started 270 simultaneously live `sleep` children and reaped them all. |
| 4 GiB storage profile | Not met by tested configuration | Root filesystem reported 20,466,256 KiB and Docker volume 10,218,772 KiB total blocks. No disk-filling test was run. |
| CLI-client loss | Recoverable in sampled test | Killed the A-side CLI; A remained listed as running with an observable VM process. No claim of native harness resume is made. |
| Explicit VM stop | Passed | Stopping A removed its observed host VM shim process and reported `stopped`, while B's live command continued. Observation did not execute inside A and accidentally restart it. |
| Final cleanup | Passed | Removed exactly the three named test sandboxes. Final sandbox list was empty and no VM shim process remained. |

`curl` without `--fail` returns exit zero for HTTP 403. The probe recorded the HTTP status rather than interpreting exit zero as successful egress.

The writable cgroup mount and guest-root access mean a guest-owned limit alone would not establish an adversarially enforced process ceiling. CPU and memory were specified at VM creation; unlimited inner cgroups do not by themselves disprove those VM limits.

## Reproduction and retained artifacts

The scratch experiment lives at `.scratch/sbx-qualification-20260911/`, outside production dispatch. It contains the synthetic source repository, input and output bundles, bare quarantine repository, and bounded guest/process/recovery probes. No raw provider credentials or personal file contents were used.

The create command shape was:

```sh
sbx create --name tyrion-qual-20260911-a \
  --cpus 2 --memory 2g --deny-network '**' --no-share-skills shell
```

Omitting the workspace argument is intentional. Do not replace it with `sbx run shell` in the Principal checkout. The test used distinct names ending in `-a`, `-b`, and `-v`; all have been removed. On rerun, choose fresh names and update the recovery probe's independently observed VM container identity. The stored recovery script is a record of this run, not a general-purpose launcher.

Input base commit: `035d859` (abbreviated local Git display).
Candidate commit: `0027d6edc0fee6a7cf5a7e80ad7fc0857bda43e6`.

| Artifact | SHA-256 |
| --- | --- |
| `host-canary.txt` | `fdf2c391b9a4067bca881997a1dce75c6fe1916844f64af9828be6e5f76c3541` |
| `input.bundle` | `bd5ca28b985274da68e5b374156b86c71e2d2529209eeda4a934c827cf345dec` |
| `candidate.bundle` | `cee4001086d35628b17da374f6fc7e134aca9425d3a5beb345e8bd215e8aadd7` |

The synthetic source repository remained clean after the experiment. Exported bundles survive VM deletion; other guest-only data was disposable and is no longer recoverable.

## What remains unqualified

This run did not test raw UDP/ICMP, controlled redirect/destination-confusion attacks, symlink/hard-link transfer attacks, root-level VM escape, malformed or unauthorized bundle rejection through Tyrion's production seam, hard storage exhaustion, model-spend enforcement, provider authentication, native structured harness I/O, approval authenticity, learned context, consequential effects, or a daemon crash. It tested local CLI-client loss and explicit stop, not every recovery case.

The shell sandboxes sometimes appeared stopped between commands; `sbx exec` restarted them with persisted files. A future adapter must establish the intended active-session lifecycle and must never use exec as a post-stop liveness probe.

Before production replacement, resolve the resource contract and immutable image identification, then prove one real Worker through the existing adapter protocol. Keep the current OpenShell backend and all production eligibility checks unchanged until that work passes. A basic sandbox smoke is not the combined Commission required by issue 16.

# Security

A coding agent is a program that can write code and run it. Tyrion's position
is simple: **contain effects, not cognition.** Each agent keeps its model, its
tools and its judgement. What it cannot keep is access to your machine.

## One disposable container per Worker

Every Worker, every check, and every comparison runs in its own fresh Docker
container, deleted afterwards. The Docker daemon sets every limit from
outside, where nothing inside the container can raise it.

| Control | How | What it means |
| --- | --- | --- |
| Processes | `--pids-limit 256` | A fork bomb stops at 256 |
| Memory and files | `--memory 3072m --memory-swap 3072m` | One hard ceiling over memory and written files together |
| Written files | a 2 GiB tmpfs at `/sandbox` | The only writable place |
| Everything else | `--read-only` | Nothing outside `/sandbox` can change |
| CPU | `--cpus 2 --cpuset-cpus <two least-loaded>` | At most two cores, spread across the machine's least busy CPUs |
| Privilege | `--cap-drop ALL --security-opt no-new-privileges --user 65534:65534` | Not root, no capabilities, no escalation |
| System calls | `--security-opt seccomp=builtin` | Filtered. Docker Desktop leaves this off by default, so Tyrion always sets it |
| Your files | no bind mount of any kind | Your home, your checkout, SSH keys and the Docker socket do not exist inside |

Code goes in and results come out as Git bundles streamed over `docker exec`.
The Worker never sees your checkout. Tyrion checks every bundle before
accepting it:

- linear history from the approved base
- only the approved files changed
- no symlink pointing outside the repository

Before any agent starts, a preflight inside the container proves every row of
that table. It reads the real limits and checks privilege, and it confirms
that your checkout, Tyrion's own state, the container socket, login folders
and credential variables are all absent. A failed preflight stops the Worker
before it runs.

## Network: its own provider, and nothing else

A Worker has no network unless its model needs one. When it does, it gets:

- a private network with no route out
- one relay per allowed destination, pinned to exactly one `host:port`

The relay forwards encrypted traffic without opening it, so no certificate is
swapped and nothing is decrypted in between.

Destinations are scoped to the harness that needs them. A Codex or OpenCode
Worker reaches `chatgpt.com` and `auth.openai.com`; a Claude Worker reaches
`api.anthropic.com`; neither reaches the other's provider or anything else.

## Credentials

Having a credential on your machine is never permission to use it.

- **Claude Workers** receive only the credential variables you configured.
  The value never appears in a file, a command line, or Tyrion's records:
  Docker reads it from the daemon's own environment when it starts the Worker.
- **Codex and OpenCode Workers** receive only the four token fields of your
  Codex login. They are streamed from memory into the container and never
  written to disk on the host.
- **Effects that need a credential**, such as calling an API on your behalf,
  go through a separate broker backed by the macOS Keychain. Workers never
  see those credentials. See [Effects](reference/effects.md).

A model credential inside a Worker is a credential that Worker can use at its
provider. Tyrion pins where it can be sent; it cannot bound what you spend
there. **Your provider's spending cap is the spending control.**

## Approvals

Anything consequential waits at an Approval Gate, such as writing a file
outside the job or calling an outside service. You approve the exact target,
content and limits with a Principal credential that only you hold. The agents
and your harness session never receive it. A changed request needs a new
approval, and an effect is never retried blindly: if its outcome is
uncertain, Tyrion stops and asks.

## Tested by attacking it

These are claims only because they were attacked:

- **Qualification:** every limit above was measured from inside a container. A fork bomb stopped
  at 256; memory, disk and CPU held; and guest root could not raise any of
  them.
- **Live Workers:** during a real
  job, 26 attacks were run inside each of three running Worker containers,
  and none reached anything. They tried becoming root, mounting, reading your
  home and checkout, writing system folders, raising their own limits,
  finding credentials, and reaching arbitrary hosts or the other provider.
- **Every job:** a test fingerprints every file, mode and byte of your
  checkout before and after a full job, and requires them identical.

## What it does not claim

- **Sibling Workers are separated by container walls, not virtual machines.**
  On macOS, Docker's virtual machine protects your Mac. Running Colima or Lima
  with no host file sharing narrows the remaining gap further.
- **Tyrion does not bound model spend.** No harness offers a hard monetary
  ceiling, so Tyrion reports cost rather than pretending to enforce it.
- **It does not defend against you, your operating system, or other programs
  running as you.** It protects your machine from the agents, not from
  itself.

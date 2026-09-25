# Tyrion

Tyrion runs coding agents for you, in parallel, and only accepts work it can prove.

You describe a job once, in your own words, in the harness you already use. Tyrion
splits it up, runs the pieces across Claude Code, Codex and Pi at the same time,
keeps each one in a sealed container, checks the result against criteria you set,
and hands you a Git branch to review. You never supervise a Worker window and you
never write JSON.

If it cannot prove the work is done, it tells you exactly what is blocking it.

## Why

Running one coding agent well means watching it. Running three means watching
three, then reconciling their edits by hand, then reading every line because the
agent's confidence tells you nothing about whether the code works. Meanwhile it
has your filesystem, your shell and whatever credentials are in your environment.

Tyrion takes the part agents are structurally bad at: authority, custody,
evidence and recovery. The agents keep the part they are good at. It contains
effects, not cognition, so each harness keeps its own model, tools and Skills.

## What it looks like

From inside Claude Code or Codex, in normal conversation:

> Add an `/auth` endpoint with tests, and keep the handler under 100 lines.

Tyrion accepts the job, runs it under containment, verifies it twice, and reports:

```
commission : verified_complete
worker     : Arya     codex   gpt-5.6-sol                succeeded  20652ms
worker     : Brienne  claude  claude-haiku-4-5-20251001  succeeded  20057ms
result     : accepted | changed ['alpha.py']
result     : accepted | changed ['beta.py']
evidence   : candidate  passed
evidence   : integrated passed
```

Then you review it like any other change:

```sh
git fetch "$TYRION_DATA_DIR/integrations/$COMMISSION_ID/repository" tyrion-integration
git diff HEAD FETCH_HEAD
git merge --ff-only FETCH_HEAD
```

**Your working copy only changes when you run that last command.** Tyrion merges
into a repository it owns, never yours.

## What is proven

Every claim below was measured against real models, with a checked-in record in
[`docs/dogfood-records/`](docs/dogfood-records/).

- A Commission driven end to end from an Entry Session, the same MCP interface
  Claude Code holds open.
- Codex and Claude running **concurrently** on disjoint work, producing one
  verified integrated artifact and beating serial execution by 20.1 seconds.
- Candidate and integrated verification, each in a separate fresh container.
- Interruption, and restart recovery against a Worker container genuinely
  orphaned by killing the daemon mid-Attempt.

The containment boundary was attacked rather than described. Every escape
attempt was blocked: reading your home directory, the host mounts, the runtime
socket, writing outside the sandbox, becoming root, mounting, unsharing. The
probe is `.scratch/docker-qual-20260921/escape.sh` and the results are in
[the qualification](docs/prototypes/docker-containment-qualification.md).

A test fingerprints every path, mode and content digest in your checkout across
a whole Commission and requires them identical afterwards.

## What it deliberately does not claim

- **Sibling Attempts are isolated at namespace strength, not VM strength.** Your
  Mac is protected by the hypervisor; two agents are separated by container
  walls.
- **Tyrion does not bound model spend.** No harness gives it a hard monetary
  ceiling, so it reports cost rather than pretending to enforce one. The cap you
  set at your provider is the control.
- **Local only.** One daemon, one machine, no cloud execution.
- The record exporter reports readiness as `blocked` or `unassessed`. It never
  certifies itself ready.

## Requirements

- macOS on Apple silicon
- Docker (Desktop, Colima or Lima)
- Rust toolchain
- A subscription or API access for at least one of Claude Code, Codex or Pi

## Setup

```sh
git clone https://github.com/aneesh-sathe/tyrion && cd tyrion
cargo build

# build the Worker image your agents run inside
docker build -f runtime/docker/Dockerfile -t tyrion-worker:local .

# generate a pinned runtime configuration for this machine
runtime/docker/generate-config.sh \
  --image tyrion-worker:local \
  --out .scratch/runtime \
  --claude /path/to/claude-linux-arm64
```

The generator discovers everything Tyrion verifies at startup: binary hashes,
the Docker CLI identity, the image identity, and each harness version, which it
reads by running the binary **inside the hardened container**. Do not hand-write
this file. A wrong digest makes the daemon refuse to start, which is correct but
opaque.

See [`runtime/docker/README.md`](runtime/docker/README.md) for each field, and
[Contained Codex Git assignments](docs/contained-codex.md) for the full
containment profile.

A single `brew install` plus `tyrion init` is tracked in
[#26](https://github.com/aneesh-sathe/tyrion/issues/26).

## Run it

```sh
target/debug/tyriond \
  --data-dir .scratch/tyrion-data \
  --socket .scratch/tyrion-data/tyrion.sock \
  --codex-worker-config .scratch/runtime/codex-worker.json \
  --worker-catalog .scratch/runtime/worker-catalog.json &

target/debug/tyrion claude     # or: tyrion codex, tyrion pi
```

That launches your normal harness with Tyrion attached. Describe the job and it
takes over from there.

## Where it is going

Tyrion is becoming a **software factory manager**: you hand it a specification,
it decides how to break the work apart, how many agents that is worth, and runs
them. The plan is in
[`docs/plans/software-factory.md`](docs/plans/software-factory.md) and the work
is issues [#22](https://github.com/aneesh-sathe/tyrion/issues/22) through
[#28](https://github.com/aneesh-sathe/tyrion/issues/28).

## More

- [Reference](docs/reference.md): vocabulary, authority model, control commands
- [Containment qualification](docs/prototypes/docker-containment-qualification.md): what was measured, and how
- [Dogfood readiness](docs/dogfood-readiness.md): the honest state of the proof
- [Issue 1](https://github.com/aneesh-sathe/tyrion/issues/1): the full product definition

Tyrion is built for one person on one machine. It is not a multi-user service, a
workflow engine, or a claim that an agent can safely act without bounded
authority and independent checks.

## Verify the repository

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The default suite uses deterministic protocol fakes for external harnesses.
Fakes are a convenience, not evidence: running real harnesses for the first time
surfaced ten defects the green suite could not, every one because a fake agreed
with the adapter instead of behaving like the real thing. Where the two now
disagree, the fake was changed.

## License

[MIT](LICENSE)

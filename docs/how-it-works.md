# How Tyrion works

Start with the [README](../README.md). This page follows one job from the
sentence you type to the branch you merge.

## One job, start to finish

1. **You ask.** In Claude Code or Codex, launched through `tyrion claude` or
   `tyrion codex`, you describe the job in your own words.
2. **Your harness drafts a proposal.** It turns your words into a precise
   proposal:
   - the goal
   - the checks that will prove it is done
   - the files agents may change
   - the time and storage limits

   It sends the proposal to Tyrion, and nothing runs yet.
3. **Tyrion plans.** Independent parts become separate pieces of work that can
   run at once. Parts that depend on each other wait their turn. If you ask,
   a planning agent drafts the plan and Tyrion checks it before anything
   writes.
4. **Agents work in sealed rooms.** Each piece goes to the harness best suited
   to it, running inside its own disposable container with a copy of your
   code and nothing else. Your checkout is never touched.
5. **Tyrion checks every result twice.** First on its own, in a fresh
   container. Then again after it is merged with everyone else's work, in
   another fresh container. An agent saying "done" counts for nothing; only
   the checks do.
6. **You review one branch.** When every check passes, Tyrion reports the job
   complete and gives you the exact commands to fetch, diff and merge.

If something fails, Tyrion keeps the evidence and picks a concrete next step:

- retry a passing hiccup once
- hand the work to a better-suited agent
- create a job to reconcile conflicting changes
- stop and tell you exactly what it needs

It never quietly gives up, and never quietly declares victory.

## The words Tyrion uses

The output uses a small, precise vocabulary. Learn these and everything
else reads plainly.

| Word | Meaning |
| --- | --- |
| **Commission** | One job you delegate: its goal, checks, limits, plan, history and outcome |
| **Assignment** | One planned piece of a Commission |
| **Worker** | One agent working one Assignment, in its own container. Workers get short names like `Vega` or `Rigel` so you can steer them |
| **Result** | What a Worker hands back. It is only a candidate until the checks pass |
| **Evidence** | The recorded outcome of a check: passed, failed, or uncertain, and why |
| **Verified Completion** | Every check passed on the merged result |
| **Blocker** | The one thing Tyrion needs from you to continue |
| **Entry Session** | Your Claude Code or Codex session, attached to Tyrion |
| **Principal** | You: the person who approves the job and anything consequential |

## Authority is granted, not assumed

An agent is never trusted because it is capable. What a Worker may do is the
overlap of three things:

- what its harness can technically do
- what the Commission you approved allows
- what its current, expiring grant covers

Anything consequential stops at an **Approval Gate**. That covers writing a
file outside the job, or calling an outside service. The approval binds the
exact target, content, limits and revision. Change any of them and the
approval no longer applies.

Only you can approve, with a Principal credential the daemon hands out exactly
once over a private pipe (`tyriond --principal-control-bootstrap-fd`). The
agents and the harness session never hold it. See
[Security](security.md).

## Watching and steering

`tyrion_status` in your Entry Session shows a compact digest:

- anything that needs you
- one line per Worker: its state, activity, time and cost
- how many checks have passed

Ask for detail and you get everything:

- the plan and its revisions
- why each Worker was routed where it was
- every Result, check and recovery

You can steer a running Worker with a clarification, or interrupt it. Neither
can change the goal, the checks or the limits you approved.

Behind the scenes it's the same public CLI you can use directly:

```sh
tyrion commission inspect COMMISSION_ID
tyrion worker steer COMMISSION_ID Vega --clarification "Keep the public API unchanged." ...
tyrion worker interrupt COMMISSION_ID Vega --reason "Stop and let me re-plan." ...
tyrion commission export-record COMMISSION_ID > record.json
```

Run `tyrion --help` for the full command tree.

## Tyrion learns how you build

Tell Tyrion a preference once, for example "Give every public function a
one-line docstring.", and it reaches every later Worker in that project as
advisory context. Your current instructions, checks and limits always win over
anything learned.

Every job's report includes a receipt for each preference an agent received,
and whether that agent's work was accepted. You can inspect, correct, suppress
or forget any learned preference, and export all of it.

## Records you can check

Every job can be exported as a single JSON record with a SHA-256 checksum:

- what you approved
- every routing decision, Result and check
- every approval and recovery
- a report that keeps approvals, interventions, corrections, timing and cost
  apart

The record never certifies itself: its `readiness` status is only ever
`blocked` or `unassessed`. The judgement is yours.

## Running the daemon yourself

`tyrion claude` and `tyrion codex` start the daemon for you with the setup
`tyrion init` wrote. To run it by hand:

```sh
tyriond \
  --data-dir ~/.local/state/tyrion \
  --socket ~/.local/state/tyrion/tyrion.sock \
  --codex-worker-config ~/.local/state/tyrion/runtime/worker-runtime.json \
  --worker-catalog ~/.local/state/tyrion/runtime/worker-catalog.json
```

The daemon refuses to start on anything it cannot verify. That covers the
Docker CLI, the Worker image and each harness version, and it applies to
every configured Worker. [`runtime/docker/README.md`](../runtime/docker/README.md)
explains each field.

## Going deeper

- [Security](security.md): the container boundary, and how it was attacked
- [Capacity](capacity.md): how many Workers one machine runs, and why
- [Routing](reference/routing.md): how Workers are chosen, and the adapter
  contract every harness meets
- [Effects](reference/effects.md): approved actions that need a credential
- [OpenCode](reference/opencode.md) and [Pi](reference/pi.md): harness notes
- [Results](results.md): the recorded runs behind every claim

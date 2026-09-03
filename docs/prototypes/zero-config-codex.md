# Zero-config Codex Commission prototype

This throwaway prototype asks whether `tyrion codex` can synthesize all
session-local Worker inputs and complete one tiny Git Commission without
user-authored JSON or persistent harness configuration, assuming Tyrion owns a
vetted contained runtime bundle.

Run it from the repository root:

```sh
cargo run --bin tyrion-zero-config-prototype
```

The command creates a disposable Git repository and Tyrion data directory under
`.scratch`, launches the real `tyrion codex` path against a fake native Codex
client, and drives the real MCP, daemon, SQLite, routing, Git bundle,
verification, and integration code. Worker execution uses the repository's
OpenShell and Codex fixtures, so it incurs no model charge and does not attest a
real MicroVM boundary. The scratch directory is deleted when the process exits.

The prototype deliberately reports two verdicts separately:

- Whether zero-configuration orchestration works when Tyrion owns the runtime.
- Whether the current machine has the pinned production runtime needed to make
  a real containment claim.

No Claude, Codex, Git, or OpenShell configuration file outside the disposable
scratch directory is created or changed.

## Verdict

The fixture-backed run passed on 2026-09-03. `tyrion codex` attached through
the real MCP path, accepted a zero-spend Git Commission, produced a verified
integrated revision, left the Principal checkout unchanged, and deleted all
three fixture sandboxes. The disposable scratch tree and all child processes
were gone after exit.

The design is viable if Tyrion owns a vetted runtime bundle and generates its
session-local inputs. Ambient host executables are not enough: the tested
machine had Codex `0.150.1` but no OpenShell, and the existing production
contract requires repaired OpenShell `0.0.104` plus a pinned Linux Codex
`0.147.0` Worker artifact. Distribution and discovery of that bundle is the
remaining production problem.

Primary source: local branch `prototype/zero-config-codex`, created for
[issue #16](https://github.com/aneesh-sathe/tyrion/issues/16).

# Credentialed effects

Tyrion can perform one exact credentialed HTTP effect while keeping the credential outside its SQLite state, Workers, and Entry Sessions. This path currently targets Apple Silicon macOS and uses macOS Keychain as the operating-system credential store.

## Trust boundary

Start `tyriond` with `--credential-runtime credential-runtime.json`. The runtime file identifies:

- an absolute, SHA-256-pinned `/usr/bin/security` binary, Keychain path, and service;
- an absolute, SHA-256-pinned `curl` binary and a closed map of destination aliases to exact HTTPS origins;
- for exceptional exposure only, the repaired OpenShell 0.0.104 source revision and patch, pinned runtime artifacts, base image, gateway and kernel configurations, hard policy, adapter, and fixed 2-vCPU, 2-GiB, 4-GiB, 256-process profile.

The daemon validates every pin at startup. It validates the broker binaries again immediately before use. One-shot execution additionally revalidates the full Effect Sandbox profile, including Landlock, seccomp, PID cgroup, network destination, allowed binaries, source repair, and adapter version.

Provision a short-lived credential under the configured Keychain service and an opaque account name. The account name becomes `credential_reference`; the secret value never enters a Tyrion request or database. The configured Keychain should be dedicated to the credential broker and protected for the daemon identity.

## Authorization flow

The current Assignment must have `credential.http.request` or `credential.command.request`, the exact destination alias, and `external.write` in its Authority Envelope. Its paid-service and storage reservations must also cover the requested operation.

The Principal first issues a revision-bound grant:

```json
{
  "assignment_id": "ASSIGNMENT_ID",
  "attempt_id": "ATTEMPT_ID",
  "worker_lease_id": "WORKER_LEASE_ID",
  "mandate_revision": 1,
  "plan_revision": 1,
  "credential_reference": "release-token",
  "capability": "http_bearer",
  "destination": "release-api",
  "exposure": "brokered_only",
  "credential_expires_at": 1787616000,
  "revocation": "delete_from_keychain"
}
```

```sh
printf '%s\n' "$TYRION_PRINCIPAL_CONTROL_TOKEN" | \
  target/debug/tyrion --socket "$TYRION_SOCKET" \
  --principal-token-stdin principal grant-credential COMMISSION_ID \
  --file credential-grant.json \
  --expected-revision CURRENT_REVISION \
  --idempotency-key grant-release-token
```

The expiry must be in the future and no more than 15 minutes away. `capability` is currently exactly `http_bearer`; revocation is exactly `delete_from_keychain`. Use `one_shot` exposure only when the typed broker cannot perform the required effect.

The Worker or Active Attachment then proposes an exact operation using the returned grant ID:

```json
{
  "assignment_id": "ASSIGNMENT_ID",
  "attempt_id": "ATTEMPT_ID",
  "worker_lease_id": "WORKER_LEASE_ID",
  "mandate_revision": 1,
  "plan_revision": 1,
  "operation": "credential.http.request",
  "repository": null,
  "target": "effects/release",
  "parameters": {
    "body": "{\"release\":\"v1\"}",
    "content_type": "application/json",
    "method": "POST",
    "reconciliation_target": "effects/release",
    "confirmed_reconciliation_sha256": "b4267ce93cbba4e415504feb895f662dcced5d7aa406f27e8447c8fd5f0d48c8",
    "not_applied_reconciliation_sha256": "9202dffe0b6057147998c5765b6c78028f6f99987d83878470bde31f404283a0"
  },
  "destination": "release-api",
  "effect": "external.write",
  "credential": {
    "grant_id": "CREDENTIAL_GRANT_ID",
    "mode": "brokered"
  },
  "consequences": ["Create the exact release marker"],
  "limits": {
    "max_output_bytes": 4096,
    "max_duration_seconds": 10,
    "max_paid_service_spend_cents": 0
  }
}
```

The operation always opens an Approval Gate. The Principal must inspect and approve its canonical digest before the Active Attachment can execute it. A `credential.command.request` uses `mode: one_shot_exposure`; approving that digest also creates a single-use Credential Exposure Grant.

Immediately before execution the daemon rechecks the exact Credential Grant, current Commission and Assignment authority, Attempt, Worker Lease, mandate and plan revisions, destination, effect, operation parameters, consequences, elapsed time, storage, and paid-service grants. The Approval Gate, Credential Grant, and optional exposure grant are consumed atomically with the durable `operation_started` event.

## Delivery and cleanup

Brokered HTTP sends the bearer header to pinned `curl` through its configuration on standard input. One-shot execution creates a fresh non-agentic Effect Sandbox with no automatic providers, uploads only the pinned adapter, and sends the credential and exact request on standard input. The credential is never passed in process arguments, environment variables, sandbox images, logs, Evidence, projections, or durable files.

Every execution bounds duration and output, scans process output and one-shot sandbox logs for the credential, retains only hashes and status metadata, deletes the Keychain item, verifies the exact Keychain not-found result, and destroys the sandbox with its descendants. Brokered curl runs in its own process group with a durable PID and host-local operation marker registered before credential delivery. The marker is not an HTTP request parameter or header. Restart finds and terminates that exact group before reconciliation. Cleanup and revocation are attempted independently; either failure makes the effect uncertain and pauses the Commission with an exact remediation requirement.

The durable journal records credential grant issuance and consumption, optional exposure authorization, operation classification and authorization, start, and the final confirmed, failed, or uncertain state under the exact governing revisions.

## Lost acknowledgement

Tyrion never automatically retries a credentialed write. If the daemon loses the acknowledgement after the destination accepted the write, restart changes the durable `started` operation to `uncertain` and pauses the Commission. An identical execute request returns the stored state without another POST.

Every credentialed request must approve both `confirmed_reconciliation_sha256` and `not_applied_reconciliation_sha256`. The Principal supplies its observation through `principal reconcile-operation`; Tyrion first confirms credential revocation and one-shot sandbox cleanup, then independently performs an unauthenticated GET against the exact approved target and requires both observations to match the selected digest. A missing cleanup runtime or failed cleanup remains an actionable blocker instead of replaying the effect.

Run the public-seam coverage on macOS with:

```sh
cargo test --test credentialed_effects
```

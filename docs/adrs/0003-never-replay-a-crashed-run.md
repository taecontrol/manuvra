# ADR-0003: Never recover a crashed run by replay

Date: 2026-09-20
Status: Accepted

## Context

Browser mutations can outlive a lost transport response or a crashed host. A persisted intent cannot prove whether the application received a submit, and a page fingerprint cannot prove external idempotency. Restarting from a job or checkpoint could therefore repeat an effect while presenting the repetition as recovery.

## Decision

Treat a crashed run as non-resumable and never recover it by replaying browser mutations. Preserve the last trustworthy checkpoint, classify an in-flight action according to the observed crash window, and close the owned browser within the lifetime bound.

Recovery means retrieving the durable result and evidence. Starting again is a new run whose effects the caller must authorize and reconcile independently.

## Consequences

- A lost host cannot silently duplicate a submit or other irreversible effect.
- Crash outcomes remain honest when dispatch cannot be proven or disproven.
- Automatic continuation after host loss is unavailable, even when replay might have been harmless.
- Callers must inspect persistence and choose whether a separate new run is appropriate.

## Alternatives considered

- **Replay from the last checkpoint:** rejected because checkpoints do not establish application-level exactly-once effects.
- **Reconnect to a leftover browser:** rejected because the new host cannot reconstruct the original in-memory authority and transport state safely.

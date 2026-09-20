# ADR-0006: Resume authority preserves code verification

Date: 2026-09-20
Status: Accepted

## Context

An uncertain run needs a caller decision to continue, but caller intent cannot make stale state, a false structured condition, a failed numeric check, an exhausted budget, or a replayed mutation safe. Treating resume as an unrestricted override would move completion and safety policy out of the executor.

At the same time, asking the model to clear the same confidence gate again would make a caller's explicit operation choice ineffective.

## Decision

A resume disposition supplies only the authority its kind states. `execute` supplies operation authority for the offered candidate; `advance` supplies an attestation only where an uncertain natural-language condition or final claim permits it.

The executor always takes a fresh observation and retains code-owned done-first checks, candidate revalidation, origin and budget guards, replay prevention, structured assertions, and numeric verification. Resume never waives them.

## Consequences

- Caller judgment can resolve uncertainty without turning resume into an arbitrary browser command.
- Stale targets, completed steps, failed structured checks, and unsafe replays still stop continuation.
- Assisted completion is distinguishable from autonomous completion in evidence.
- Some caller-approved intentions require another escalation when the page has changed.

## Alternatives considered

- **Unrestricted override:** rejected because it would bypass the invariants that make outcomes trustworthy.
- **Re-run the operation confidence gate:** rejected because it would ignore the explicit authority the caller supplied.

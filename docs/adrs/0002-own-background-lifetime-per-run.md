# ADR-0002: Own background lifetime per run

Date: 2026-09-20
Status: Accepted

## Context

A caller can lose stdout or exit while a browser journey is paused for a disposition. The browser and its in-memory authority must remain available for bounded recovery, while unrelated runs must not share failure, upgrade, or ownership state.

A shared session/target daemon would make callers manage exported lifetime concepts and would couple otherwise independent runs. A foreground-only process would instead make recovery depend on the caller retaining a terminal handle.

## Decision

Give each run its own background host and watchdog, with the host owning the browser and the watchdog enforcing the run's pause and absolute lifetime.

The CLI starts or reconnects to that run-owned lifetime through the public run commands. No shared daemon owns multiple runs.

## Consequences

- A caller can recover a live run after losing its original invocation without sharing browser ownership across runs.
- Failure, cleanup, deadlines, and resource use are isolated per run.
- Every concurrent run pays for its own host, watchdog, and Chromium.
- Losing the run host ends resumability; bounded cleanup takes priority over reconstructing hidden state.

## Alternatives considered

- **Shared session/target daemon:** rejected because it exposes lifetime management to callers and couples independent runs.
- **Foreground process:** rejected because a paused run would depend on one terminal process remaining attached.

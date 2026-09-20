# ADR-0005: Own the browser protocol integration

Date: 2026-09-20
Status: Accepted

## Context

Manuvra needs a narrow part of the Chrome DevTools Protocol, but it also needs semantics that general browser-driver crates do not provide: explicit distinction between a command known not to have been sent and one with an unknown dispatch outcome, a bounded event journal, an owned browser lifetime, and act-time target revalidation.

Generated or high-level drivers expose a broader API, but adopting one would still require bypassing its helpers for mutation accounting, observation, input readback, and process ownership. It would also make protocol-version changes affect code outside the surface Manuvra actually uses.

## Decision

Maintain the narrow CDP transport and browser integration in `manuvra-chrome`. Keep protocol messages explicit at this boundary, especially for experimental CDP domains and methods, and expose only Manuvra's observation, input, evidence, and lifecycle semantics to the rest of the workspace.

Use an injected, indexed page snapshot as the primary observation. Use CDP DOM and accessibility queries as targeted cross-checks and evidence, not as a second source of action authority.

## Consequences

- Mutation outcomes and target freshness retain the exact semantics required by the replay policy.
- Browser process ownership and cleanup remain aligned with one run rather than with a third-party driver's lifecycle.
- Protocol updates are localized, but Manuvra owns compatibility testing and the maintenance cost of its CDP surface.
- Supporting a new browser engine requires a new integration behind Manuvra's browser boundary; it is not a driver swap.

## Alternatives considered

- **Adopt a generated CDP driver:** rejected because its broader protocol coverage does not replace Manuvra's transport journal, mutation outcomes, or lifecycle rules.
- **Use the full accessibility tree as the primary observation:** rejected because action selection still needs live DOM identity, geometry, coverage, and input state; targeted accessibility queries remain useful as cross-checks.

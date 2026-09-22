# ADR-0007: Gate CRAP per compiled platform

Date: 2026-09-21
Status: Accepted

## Context

ADR-0001 pins production CRAP at 8 with a hard local full-project gate and no waivers, while hosted CI stays advisory. ADR-0006 places platform-specific mechanics in per-platform sibling modules, so the workspace already contains production functions that only one platform compiles and can cover.

The gate tool enumerates functions from source and scores a file with no coverage records pessimistically at zero. This asymmetry predates ADR-0006: at its baseline the macOS analysis already fails 61 functions macOS cannot compile or reach, and ADR-0006's Darwin siblings make it symmetric by adding files Linux never links. Left unresolved, the hard gate cannot be green anywhere, which invites exactly the erosion ADR-0001 forbids.

## Decision

The CRAP gate is applied per platform over the code that platform compiles. Each supported platform must pass its own hard full-project analysis at threshold 8, with source that only another platform's siblings compile excluded from that platform's analysis. A function is gated on every platform that compiles it, and shared code is gated on all supported platforms.

The threshold, the no-waiver rule, and the advisory role of hosted CI in ADR-0001 are unchanged; this decision only scopes what each platform's analysis measures.

## Consequences

- Every production function keeps a hard gate on at least one platform, the one whose tests can actually cover it, and shared code stays gated everywhere.
- A change is complete only when each supported platform's own gate passes; one green platform proves nothing about the other's siblings. Hosted CI stays advisory, so this rests on reading both platforms' advisory inventories or running both gates locally.
- Coverage is recorded per file, so a misclassified excluded file and a platform-gated function left inline in a shared file both pass silently on the other platform. Platform mechanics live in sibling files per ADR-0006, the inline divergence ADR-0006 allows in `manuvra-flow` is gated only where it compiles, and exclusion changes are reviewer-judged like other gate changes.
- No cross-platform coverage merge tooling is built or maintained.

## Alternatives considered

- **Union of per-platform coverage reports:** rejected because it requires new merge tooling and an aggregation step that breaks the local single-command gate loop.
- **Hard gate on Linux only, macOS advisory:** rejected because Darwin sibling production code would carry no hard gate, contradicting ADR-0001 in practice.

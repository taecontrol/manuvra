# ADR-0003: Pin production CRAP at 8 and keep hosted CI advisory

Date: 2026-08-22
Status: Accepted

## Context

Manuvra measures function-level CRAP with pessimistic missing coverage and no waivers. At 100% coverage the score equals cyclomatic complexity, so a threshold is also a cap on how branched a production function may be.

The previous pin was 15. Hosted CI printed the inventory and allowed analyzer failure so coverage holes on Apple-only paths would not block merge. Local `make crap` and the proof schemas still treated the pin as a hard maximum.

A threshold of 15 let fully covered functions keep 9–15 branches. That hid complexity behind a passing score. Lowering the pin without first making the inventory meet it would have left the official gate red.

## Decision

Pin production CRAP at 8 in `crap-gate`, proof certificate and public-summary schemas and scripts, and CI's inventory assertion.

Keep hosted CI advisory: the job still accepts analyzer status 0 or 1. Local `make crap` and proof verification fail when any production function scores above 8.

Do not waive functions. Cover or split until every measured production function is at most 8.

## Consequences

- A fully covered function may have at most eight branches. Further branching needs a split or it fails the local gate.
- Hosted CI will not fail a pull request solely because llvm-cov missed an Apple-only path, as long as the reported threshold is 8.
- Proof certificates cannot be issued for a tree whose maximum is above 8.
- The pin is cheap to read from `ACCEPTED_THRESHOLD` and the proof schemas. Changing it again is a schema and CI contract change, not only a Makefile tweak.

## Alternatives considered

- **Keep 15:** preserves today's fully covered 9–15-branch functions, but the score no longer limits complexity once coverage is high.
- **Make hosted CI a hard fail at 8:** would block merge on coverage attribution noise for live AX/CDP paths that hosted runners cannot exercise.
- **Waive named functions:** would hide the same complexity the pin is meant to force into the open.

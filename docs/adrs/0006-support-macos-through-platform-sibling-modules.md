# ADR-0006: Support macOS through platform sibling modules

Date: 2026-09-21
Status: Accepted

## Context

Manuvra's runtime is Linux-only by design: browser launch, run hosting, and supervision are compile-gated, and every non-Linux run is blocked as `unsupported_platform`. macOS is now a required runtime with the same guarantees, while the ownership invariants of ADR-0002, ADR-0003, and ADR-0005 currently rest on Linux-specific process identity, peer authentication, and parent-loss mechanics that do not exist on Darwin.

A bounded spike on one Apple Silicon host observed Darwin equivalents for each required invariant, with signal escalation and forced PID reuse left to implementation tests, so the accepted architecture can be preserved rather than redesigned. The open conflict is structural: platform differences could spread as inline `cfg(target_os)` branches, or a weaker generic Unix fallback could be promoted to a supported runtime, and either would blur which guarantees each platform actually provides.

## Decision

A platform is supported only when its own native implementation preserves the same ownership, cleanup, identity, and authentication invariants as Linux. No generic fallback is ever presented as a supported runtime.

Platform-specific process and browser mechanics live in per-platform sibling modules behind shared platform-neutral interfaces, inside the crates that already own those mechanics (`manuvra-cli`, `manuvra-chrome`). `manuvra-contract` and `manuvra-jev` stay free of platform knowledge. `manuvra-flow` keeps only its existing per-platform support gates and evidence-publication divergence, widened per supported platform; it gains no new process or browser platform branches.

## Consequences

- Each platform's guarantees are auditable in one place per crate, and a future platform is added by writing new siblings, not by editing policy code.
- Some duplication between siblings is accepted where sharing would leak platform detail through the shared interface.
- A sibling implementation is not a supported runtime until it has native real-browser and lifecycle proof on that platform.
- The generic non-Linux fallback shrinks to the honest `unsupported_platform` path for platforms without a sibling implementation.

## Alternatives considered

- **Promote the portable POSIX fallback to a supported runtime:** rejected because killing only the direct child cannot prove helper-tree cleanup or resist PID reuse; it would silently weaken ADR-0002 and ADR-0003 on macOS.
- **Inline `cfg(target_os)` branches at each divergence point:** rejected because platform knowledge would spread through run policy and supervision logic, making per-platform guarantees unauditable.
- **A separate platform-abstraction crate:** rejected because the owning crates already exist and a shared crate would have to export process and lifetime types across a boundary that today stays private.

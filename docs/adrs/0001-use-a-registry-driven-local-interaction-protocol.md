# ADR-0001: Use a registry-driven local interaction protocol

Date: 2026-08-17
Status: Accepted

## Context

The tool has one shell-facing CLI, one long-lived daemon, one shared action engine, and two initially real target adapters: Chrome and macOS. Sessions, exclusive actor leases, cancellation, target identity, background/foreground policy, post-action evidence, artifact lifetime, errors, and compatibility must behave identically regardless of which CLI spelling or adapter handles an operation. A separate hand-written CLI grammar, daemon router, capability table, documentation set, and schema catalog would allow these guarantees to drift.

The local process seam must also remain fast and inspectable. Target adapters need freedom to expose arbitrary CDP and Accessibility operations without leaking their connection IDs, native handles, run loops, or fallback mechanics into common callers.

## Decision

Use a private, same-user Unix-domain-socket protocol whose external daemon interface is one versioned `manuvra.invoke` operation. Requests and responses use length-prefixed JSON-RPC 2.0 frames, while application commands are typed registry IDs inside the invocation envelope.

Define each command once in a versioned typed registry. Generate CLI parsing, local and daemon validation, JSON Schema, machine discovery, context-free help, defaults, authority and mode checks, timeout policy, capability requirements, examples, and compatibility fixtures from that source. Common actions remain direct CLI verbs; backend-only capability remains namespaced below `raw` and `observe`.

Keep the daemon as the deep module that owns admission ordering, sessions, leases, deadlines, cancellation, target-generation checks, result truthfulness, output budgeting, artifact lifecycle, and dispatch into a small target-adapter seam. Chrome, macOS, and deterministic fake adapters satisfy that real seam. Backend-native details and locally substitutable clock/filesystem mechanics remain internal seams rather than public interface concepts.

## Consequences

- A caller learns one factual result model and one command catalog, while complex cross-cutting behavior remains local to the daemon.
- Adding a common verb or changing its default or guarantee becomes a deliberate registry-version decision rather than an adapter-local edit.
- The registry becomes critical infrastructure and requires parity tests proving that CLI, schemas, router, help, examples, and capability projections agree.
- JSON at the process seam remains inspectable, but the daemon must validate and convert it immediately into typed Rust inputs.
- Raw commands preserve backend reach without receiving common semantic guarantees; their narrower contract must remain visible in discovery and help.
- The MVP deliberately excludes TCP/HTTP clients and batch requests. An incompatible local CLI/daemon update can require explicit session closure and daemon restart.
- A generic `invoke` or one-method adapter can become shallow if common policy leaks into callers or each adapter reimplements it. Tests and ownership rules must keep admission, stabilization orchestration, result construction, and artifact policy in the daemon/action engine rather than in CLI or adapters.

## Alternatives considered

- **One generic `session run` CLI command:** minimizes grammar but makes the common caller construct tagged operations and learn more schema mechanics for every action.
- **Fully namespaced CLI methods:** maximizes extension regularity but adds avoidable ceremony to the high-frequency agent loop.
- **Independent first-class methods across CLI, daemon, and adapters:** looks typed locally but duplicates lifecycle and policy knowledge across shallow interfaces, making drift likely.
- **HTTP or TCP JSON-RPC:** would enable remote clients that are outside the MVP while increasing authentication and exposure surface.

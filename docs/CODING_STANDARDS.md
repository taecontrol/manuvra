# Coding standards

These standards apply to production code, tests, scripts, and maintained documentation in this repository.

## Boundaries and vocabulary

- Preserve the dependency direction between workspace crates. Put wire DTOs in `manuvra-contract`, browser mechanics in `manuvra-chrome`, provider transport in `manuvra-jev`, run policy in `manuvra-flow`, and process, persistence, and CLI concerns in `manuvra-cli`.
- Use the domain terms in [`CONTEXT.md`](../CONTEXT.md). Do not introduce synonyms for established concepts in public types, evidence, or maintained documentation.
- Keep provider judgments advisory. Completion, mutation authorization, replay prevention, origin checks, budgets, and verification remain deterministic code-owned policy.
- Keep public contracts free of CDP node ids, sockets, process ids, prompt text, and other implementation details.

## Safety and persistence

- Never infer that a browser mutation was not performed when dispatch may have occurred. Preserve `observed`, `not_performed`, and `uncertain` outcomes through every boundary.
- Never retry or replay a mutation unless code has proved non-effect and the existing policy explicitly permits the bounded fallback. Follow [ADR-0003](adrs/0003-never-replay-a-crashed-run.md).
- Revalidate browser identity, state, origin, budgets, and replay guards immediately before dispatch. A caller disposition does not waive verification; follow [ADR-0004](adrs/0004-resume-authority-preserves-verification.md).
- Redact secrets and classified values before persistence or provider calls. Write private evidence atomically, publish digests and completeness truthfully, and fail closed when required evidence cannot be verified.

## Rust and interfaces

- Prefer small functions with one policy responsibility. Production functions must satisfy the repository's CRAP threshold of 8 with no waivers; follow [ADR-0001](adrs/0001-pin-production-crap-at-8.md).
- Model closed states and outcomes with enums and validate untrusted input at the owning boundary. Reject unknown input fields where the versioned contract requires it.
- Return structured errors across production boundaries. Reserve panics and unchecked assumptions for tests or invariants that are locally proved and explained.
- Keep platform-specific code behind narrow interfaces and keep the macOS compilation path intact when changing Linux browser behavior.

## Tests and gates

- Add the smallest deterministic test at the boundary that owns changed behavior. Use real Chromium fixtures when correctness depends on CDP events, geometry, input behavior, or browser lifetime.
- For mutations and persistence, test success plus ambiguous dispatch, interrupted write, stale state, and cleanup paths that the change can affect.
- Do not tune away or discard the first failing live run. Classify autonomous, assisted, stopped, and failed outcomes truthfully.
- Before completion, run `make fmt`, `make lint`, `make test`, and `make crap`. Run the relevant live matrix when browser integration or end-to-end policy changes.

## Maintained artifacts

- Name durable files by their long-lived purpose, never by an implementation slice, temporary phase, or delivery sequence.
- Put costly-to-reverse architectural rationale in `docs/adrs/`, accepted domain vocabulary in `CONTEXT.md`, agent instructions in `AGENTS.md`, and current operator usage in `README.md` or the installed skill.
- Do not retain research notes, implementation plans, validation scratch data, or superseded design documents as maintained documentation. Promote only the durable conclusion to its owning artifact.

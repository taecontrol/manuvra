# Repository instructions

Before changing this repository:

- Read [`CONTEXT.md`](CONTEXT.md) and use its domain vocabulary.
- Follow [`docs/CODING_STANDARDS.md`](docs/CODING_STANDARDS.md).
- Consult [`docs/adrs/`](docs/adrs/) before changing an architectural invariant.

## Measurement

Manuvra's users are its author and the author's coding agents. Prove a feature with lab evidence before release: deterministic tests, the CRAP gate on each supported platform, real-browser fixtures, and live suites against synthetic fixtures. Do not add production metrics or post-release tracking; success is continued real use. Accessibility budgets do not apply because Manuvra has no UI.

Keep temporary plans, research, validation evidence, and delivery-stage names out of durable files. When temporary work establishes a lasting rule, move only that rule to the durable artifact that owns it.

Publish a version only by dispatching [`.github/workflows/release.yml`](.github/workflows/release.yml) from `main` after CI succeeds for that exact commit. Never create a release tag, publish a GitHub release, or upload release assets manually; the workflow owns the tag, source and native binary assets, attestations, `mise` installation checks, and Homebrew update.

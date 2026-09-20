# Manuvra

Manuvra is a browser flow executor for coding agents. A caller submits a versioned JSON job containing a start URL, English steps, named values, done conditions, and final expectations. Manuvra controls a dedicated Chromium, asks TypeSafe Jev for typed judgments, applies code-owned safety policy, and returns one JSON result plus an absolute evidence-manifest path.

Linux is the supported execution platform. The workspace also keeps its non-Linux fallback compiling on macOS.

## Build and install

The workspace requires Rust 1.95 or newer.

```bash
cargo build --release --locked
cargo install --path crates/manuvra-cli --locked
manuvra version
```

Runtime browser discovery checks `--browser`, then `MANUVRA_BROWSER`, then supported Chromium/Chrome locations. Headed execution uses the current Wayland/X11 desktop; pass `--headless` for a headless run.

## Environment

Set `TYPESAFE_API_KEY` for jobs that need Jev judgments. Manuvra keeps the key in memory, removes it from the browser environment, and redacts classified values before evidence is written.

`XDG_STATE_HOME` selects durable run records and falls back to the user's XDG state directory. `XDG_RUNTIME_DIR` is required for private runtime control state. Callers own the application fixture and the evidence root supplied to each run.

## Commands

Inspect the versioned input and output contracts before authoring jobs:

```bash
manuvra schema job
manuvra schema result
manuvra schema disposition
manuvra schema manifest
```

Start a run and wait for its next checkpoint:

```bash
manuvra run \
  --request-id account-check-1 \
  --job /tmp/create-account.json \
  --evidence /tmp/manuvra-evidence \
  --wait-ms 30000
```

Every invocation writes exactly one JSON object to stdout. The JSON is authoritative; exit codes are `0` passed, `2` uncertain, `3` blocked, `4` failed, `5` aborted or expired, `6` running, `64` invalid input or request conflict, and `70` an internal failure without a recoverable result.

Recover a result after lost stdout or wait for a running host:

```bash
manuvra status --request-id account-check-1
manuvra status r_example --wait-ms 30000
```

An uncertain result lists the dispositions valid for that escalation. Submit only one of those choices using the disposition schema:

```bash
manuvra resume r_example \
  --request-id account-check-1-resume-1 \
  --input /tmp/disposition.json
```

Stop a live run explicitly when it should not continue:

```bash
manuvra abort r_example --request-id account-check-1-abort
```

Request ids are idempotency keys. Reusing one with identical input recovers the same result; reusing one with different input is a conflict.

## Evidence

Each run owns a private directory beneath the requested evidence root. Its `manifest.json` lists every artifact by role, absolute path, digest, and completeness. Evidence includes the normalized redacted job, provenance, incremental results, observations and screenshots, decisions, step facts, action trace, escalations and dispositions, final verification, and cleanup state when those artifacts apply.

A `passed` result requires complete evidence. Consumers should still verify the manifest and artifact digests, inspect the first divergence or escalation, and confirm persisted effects through an application-owned seam. Manuvra's verdict is evidence for product validation; it is not itself a Product Validator Pass.

## Development and live validation

```bash
make fmt
make lint
make test
make crap
```

`make live` runs the release-build money journey matrix against fresh fixtures. It requires Chromium, a headed desktop, `TYPESAFE_API_KEY`, `jq`, and the Money repository at `/home/guetteluis/Work/personal/money` (override with `MONEY_DIR`). The matrix runs create-unit, create-account, and record-transaction three times each, plus one forced escalation round-trip. Timestamped evidence and its report are written under `.work/live/money-journey/`.

The maintained usage recipe for coding agents is [skills/manuvra/SKILL.md](skills/manuvra/SKILL.md). Contributors and coding agents should follow [docs/CODING_STANDARDS.md](docs/CODING_STANDARDS.md). Design rationale is recorded in [docs/adrs](docs/adrs/).

## License

MIT. See [LICENSE](LICENSE).

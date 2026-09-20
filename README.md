# Manuvra

Manuvra runs browser journeys for coding agents. A JSON job gives it a start URL, ordered goals, named values, completion conditions, and final expectations. Manuvra opens a dedicated Chromium, asks TypeSafe Jev for typed judgments, and applies its safety rules in Rust before it changes the page. Each command returns one JSON result and a path to the run's evidence manifest.

Use Manuvra with a disposable application fixture and synthetic data. The caller starts and resets the application, defines which effects are allowed, checks persisted state, and cleans up afterward. Manuvra owns its browser process and browser evidence.

## Requirements

Manuvra runs on Linux and requires Chromium or Google Chrome. Building it from source requires Rust 1.95 or newer. Jobs that need Jev judgments also require `TYPESAFE_API_KEY`.

The workspace keeps the macOS fallback compiling, but macOS is not a supported runtime platform.

## Install

On Omarchy, install the latest Linux release with the bundled `mise`:

```bash
MISE_MINIMUM_RELEASE_AGE=0 mise use -g github:taecontrol/manuvra@latest
manuvra version
```

The same command works on other Linux systems with `mise`. Releases contain native x64 and ARM64 archives, and `mise` selects the matching one. Run `omarchy update mise` to update Manuvra along with other mise-managed tools.

If Chromium is not already installed on Omarchy, add it with:

```bash
omarchy pkg add chromium
```

To build from source instead:

```bash
cargo build --release --locked
cargo install --path crates/manuvra-cli --locked
manuvra version
```

On macOS, the same CLI is distributed through the existing Homebrew tap:

```bash
brew install taecontrol/tap/manuvra
manuvra version
```

The macOS package currently exposes `version` and the four `schema` contracts. Browser-journey execution remains unsupported on macOS; `run`, `status`, `resume`, and `abort` return `unsupported_platform`. The Homebrew release check builds and tests the formula on macOS so the install channel stays ready while runtime support is restored separately.

At runtime, Manuvra looks for the browser specified by `--browser`, then `MANUVRA_BROWSER`, then known Chromium and Chrome locations. It uses the current Wayland or X11 desktop unless you pass `--headless`.

Manuvra stores durable run records under `XDG_STATE_HOME`, or the user's standard XDG state directory when that variable is unset. It requires `XDG_RUNTIME_DIR` for private control sockets and other short-lived state. The caller chooses a separate evidence root for each `run` command.

## Write a job

Ask the installed binary for its current contracts before writing input:

```bash
manuvra schema job
manuvra schema result
manuvra schema disposition
manuvra schema manifest
```

This example creates one synthetic account in a local fixture:

```json
{
  "schema_version": 1,
  "target": {"kind": "browser", "url": "http://127.0.0.1:4351/"},
  "context": {
    "journey": "Create one synthetic account",
    "revision": "<tested commit or build id>",
    "environment": "fresh disposable fixture",
    "actor": "synthetic owner",
    "authority": "create one synthetic account in this fixture"
  },
  "values": {
    "account_name": {
      "value": "Validation wallet 7f3a",
      "description": "Unique name for the synthetic account"
    }
  },
  "steps": [
    {
      "id": "open",
      "goal": "Open the account creation dialog.",
      "done_when": [{"dialog_open": "Create account"}]
    },
    {
      "id": "name",
      "goal": "Fill Account name with account_name.",
      "requires_values": ["account_name"],
      "done_when": [
        {"field": "Account name", "equals_value": "account_name"}
      ]
    },
    {
      "id": "submit",
      "goal": "Submit Create account.",
      "done_when": [
        {"dialog_closed": "Create account"},
        {"text_visible": "Validation wallet 7f3a"}
      ]
    }
  ],
  "expectations": [
    {
      "id": "account-visible",
      "claim": "The final page shows the Validation wallet 7f3a account."
    }
  ],
  "options": {
    "allowed_origins": ["http://127.0.0.1:4351"]
  }
}
```

Write each step as one visible transition. The `goal` says what the browser should do, while `done_when` describes the state that must follow. Structured conditions can check visible or absent text, field values, open or closed dialogs, and URL fragments. Use natural-language conditions only when the structured forms cannot express the required state.

Values have stable names so Jev can select a value without inventing one. Mark sensitive values with `"secret": true`. Keep jobs that contain classified values outside the repository and restrict their file mode to `0600`.

The [Manuvra skill](skills/manuvra/SKILL.md) contains the full job-authoring and recovery procedure for coding agents. The schemas printed by the installed binary remain the authority for accepted fields and limits.

## Start a run

Give every invocation a request id. The id lets you recover the same response if stdout is lost.

```bash
manuvra run \
  --request-id account-check-1 \
  --job /tmp/create-account.json \
  --evidence /tmp/manuvra-evidence \
  --wait-ms 30000 > /tmp/account-check-1-result.json

jq . /tmp/account-check-1-result.json
```

Every invocation writes exactly one JSON object to stdout. The JSON is authoritative. Exit codes classify the checkpoint:

- `0` means passed.
- `2` means uncertain and waiting for a disposition.
- `3` means blocked.
- `4` means failed.
- `5` means aborted or expired.
- `6` means still running.
- `64` means invalid input or a request conflict.
- `70` means an internal failure occurred before Manuvra could publish a recoverable result.

## Follow or resume a run

A result has a `run_id`, `state`, and `terminal` flag. A `running` result means the host still owns the browser. Wait for another checkpoint with:

```bash
manuvra status r_example --wait-ms 30000
```

If the original command's stdout was lost, recover its run through the request id:

```bash
manuvra status --request-id account-check-1
```

An `uncertain` result includes an escalation payload and the dispositions allowed at that point. Inspect the payload and choose only one of the listed dispositions. Submit it with a new request id:

```bash
manuvra resume r_example \
  --request-id account-check-1-resume-1 \
  --input /tmp/disposition.json
```

The disposition schema supports four choices. `execute` authorizes one offered candidate. `advance` attests an allowed natural-language condition and requires a rationale. `retry_observation` asks Manuvra to inspect the page again. `abort` ends the run. Manuvra rechecks page state and its safety rules before it acts on caller authority.

Stop any live run directly when it should no longer continue:

```bash
manuvra abort r_example --request-id account-check-1-abort
```

Request ids are idempotency keys. Reusing an id with the same input recovers the existing response. Reusing it with different input returns a conflict.

## Verify the result

Each run has a private directory beneath the requested evidence root. Its `manifest.json` lists every artifact by role, absolute path, SHA-256 digest, and completeness. Depending on the run, evidence includes the redacted job, provenance, checkpoints, observations, screenshots, decisions, step facts, action trace, escalations, dispositions, final verification, and cleanup state.

A `passed` result means Manuvra completed the browser steps and final expectations with complete evidence. It does not prove that the application persisted the intended effect. Before treating the run as product proof:

1. Verify the manifest and artifact digests.
2. Inspect the first divergence or escalation and the final verification.
3. Query the application's own persistence layer for the expected entities and duplicate count.
4. Confirm that the browser, its profile, and the application fixture were cleaned up.

Product validation should derive Pass, Fail, or Inconclusive from those facts rather than copy Manuvra's verdict.

## Development

Run the local gates with:

```bash
make fmt
make lint
make test
make crap
```

`make live` builds a release binary and runs the Money journey matrix against fresh fixtures. It requires Chromium, a headed desktop, `TYPESAFE_API_KEY`, `jq`, and the Money repository at `/home/guetteluis/Work/personal/money`. Set `MONEY_DIR` to use another checkout. The matrix runs the create-unit, create-account, and record-transaction journeys three times each, followed by one forced escalation round trip. It writes timestamped evidence and a report under `.work/live/money-journey/`.

Contributors and coding agents should follow [docs/CODING_STANDARDS.md](docs/CODING_STANDARDS.md). The [architecture decision records](docs/adrs/) explain the project's design choices.

## Release

Releases start from the `release` workflow on `main`. Enter the workspace version from `Cargo.toml` without the leading `v`. The workflow requires a successful CI run for that exact commit, then:

1. Builds deterministic Linux x64 and ARM64 archives and publishes their SHA-256 checksums.
2. Creates GitHub build-provenance attestations for both Linux archives.
3. Publishes those binaries with the deterministic source archive.
4. Installs the published binaries through `mise` on native x64 and ARM64 runners.
5. Installs the rendered formula on macOS and opens an auto-merge pull request in [`taecontrol/homebrew-tap`](https://github.com/taecontrol/homebrew-tap).

Repository secret `HOMEBREW_TAP_TOKEN` provides write access to the tap. The release workflow does not modify Omarchy; Omarchy consumes the ordinary GitHub release through `mise`.

## License

MIT. See [LICENSE](LICENSE).

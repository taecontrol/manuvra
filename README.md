# Manuvra

Manuvra runs browser journeys for coding agents. A JSON job gives it a start URL, ordered goals, named values, completion conditions, and final expectations. Manuvra opens a dedicated Chromium, asks TypeSafe Jev for typed judgments, and applies its safety rules in Rust before it changes the page. Each command returns one JSON result and a path to the run's evidence manifest.

Use Manuvra with a disposable application fixture and synthetic data. The caller starts and resets the application, defines which effects are allowed, checks persisted state, and cleans up afterward. Manuvra owns its browser process and browser evidence.

## Requirements

Manuvra runs on Linux and macOS and requires Chromium or Google Chrome. Building it from source requires Rust 1.95 or newer. Jobs that need Jev judgments also require `TYPESAFE_API_KEY`.

The macOS runtime and real-Chrome lifecycle are proven on Apple Silicon. Intel macOS uses the same native implementation, but does not yet have an equivalent real-Chrome runtime gate.

## Install

On Omarchy, install the latest Linux release with the bundled `mise`:

```bash
MISE_MINIMUM_RELEASE_AGE=0 mise use -g github:taecontrol/manuvra@latest
manuvra version
```

The same command works on other Linux systems and macOS with `mise`. Releases contain native x64 and ARM64 archives for both platforms, and `mise` selects the matching archive. The release workflow verifies installation on native Linux and macOS runners. On Intel macOS this is an installation smoke check; the real-Chrome runtime gate covers Apple Silicon. Run `omarchy update mise` to update Manuvra along with other mise-managed tools on Omarchy.

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

Homebrew builds Manuvra from source and does not use a bottle. That installation and a source build both provide the complete `run`, `status`, `resume`, and `abort` lifecycle on macOS.

At runtime, Manuvra looks for the browser specified by `--browser`, then `MANUVRA_BROWSER`, then known Chromium and Chrome locations. On macOS it checks the directly executable Google Chrome and Chromium binaries inside `/Applications` and `~/Applications` app bundles before searching `PATH`; an explicit path must name the inner executable, such as `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`. It uses the current macOS desktop, Wayland, or X11 display unless you pass `--headless`. Headless mode presents a desktop mouse, including hover support and a fine pointer. It hides native scrollbars at every viewport size so they consume no layout width; document and nested-region scrolling remain available. Page-authored gutters and overflow still apply.

Manuvra stores durable run records under `XDG_STATE_HOME`, or the user's standard XDG state directory when that variable is unset. It uses `XDG_RUNTIME_DIR` for private control sockets and other short-lived state. On macOS, if `XDG_RUNTIME_DIR` is unset, it creates a private mode-`0700` runtime directory beneath the explicit `TMPDIR`. When no runtime directory is available (`XDG_RUNTIME_DIR` on Linux, or both variables on macOS), `run` exits with status 3 and `runtime_directory_unavailable` before creating state or children, so the same request id can be retried once the variable is set. The caller chooses a separate evidence root for each `run` command.

Install the [Manuvra skill](skills/manuvra/SKILL.md) for your coding agents with the [skills](https://skills.sh/) CLI:

```bash
npx skills add taecontrol/manuvra --skill manuvra
```

Add `-g` to install it for your user instead of the current project.

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

Write each step as one visible transition. The `goal` says what the browser should do, while `done_when` describes the state that must follow. Structured conditions can check visible or absent text, field values, open or closed dialogs, the focused element (`{"focused": "Save"}`, with optional `role` and `dialog`), and URL fragments. Use natural-language conditions only when the structured forms cannot express the required state.

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

In `provenance.json`, `viewport.width` and `viewport.height` are the requested CSS dimensions, including the default 1120×780 when the Job omits a viewport. `viewport.initial_client_width` is the target's measured `document.documentElement.clientWidth` after the start URL settles and before Step input. It is an initial sample retained through later navigation and Run recovery, not a measurement of the current page or its descendant content. It is `null` when navigation fails before measurement; a failed or invalid width read blocks the Run before its Steps. A launch failure may have no viewport provenance.

Linux replaces a previously published Evidence directory with an atomic directory exchange. macOS uses a portable backup-then-rename replacement, so an abrupt machine or process failure can temporarily leave the final Evidence path absent. Treat Evidence as complete only when the returned result and manifest both say it is complete and every listed Artifact verifies; recover the durable Run with `status` after an interrupted caller.

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

`fmt`, `lint`, and `test` also check the CRAP gate tool in `tools/crap-gate`, which is outside the workspace. `make crap` needs the Rust `llvm-tools-preview` component, `cargo-crap` 0.4.3, and `cargo-llvm-cov` 0.9.0, the versions CI installs:

```bash
rustup component add llvm-tools-preview
cargo install cargo-crap --version 0.4.3 --locked
cargo install cargo-llvm-cov --version 0.9.0 --locked
```

`make live-self-test` checks the live-suite harnesses against synthetic input without a provider key, browser, or Money checkout. CI runs it on Linux and macOS.

The live suites use real Jev judgments and a real browser against synthetic fixtures. Each needs `TYPESAFE_API_KEY` exported in the environment and retains timestamped, redacted Evidence under `.work/live/`. `make live-all` runs every suite below in sequence.

`make live` uses `/bin/bash`, builds a release binary, and runs the Money journey matrix against fresh fixtures. Set `MONEY_DIR` explicitly to a disposable Money checkout and provide `TYPESAFE_API_KEY` in the environment. The checkout must have its documented Node and pnpm application-driver dependencies ready. The command also requires `jq`, `grep`, `rg`, native `date`, `shasum`, and `nc`, Rust build tools, Google Chrome, and a headed desktop. On Linux the matrix requires `XDG_RUNTIME_DIR` and passes it to Manuvra. On macOS it requires `TMPDIR` and leaves `XDG_RUNTIME_DIR` unset so Manuvra exercises its private `TMPDIR` runtime fallback. It runs the create-unit, create-account, and record-transaction journeys three times each, one create-account journey whose account name is classified, and one forced escalation round trip. The classified Run must keep the name out of its results, state, and Evidence. It retains timestamped, redacted Evidence and a `report.json` under `.work/live/money-journey/`; the report records the source revision, release-binary digest, Bash version, Run classifications, application persistence checks, and cleanup results.

The remaining Money suites run on Linux with the same `MONEY_DIR`, `TYPESAFE_API_KEY`, and headed desktop:

- `make live-observation` runs read-only observation jobs, including classified rendered text and a provider key stand-in that must never be exported.
- `make live-natural-done` runs the create-account journey three times with natural-language done conditions. A Run passes, or stops without preparing an action the job does not intend.
- `make live-resume-dispositions` needs `XDG_RUNTIME_DIR`. It races two resumes for one escalation, then checks request replay, stale escalations, and request conflicts.
- `make live-run-lifecycle` needs `XDG_RUNTIME_DIR`. It follows one forced-pause Run through attach, expiry, retry, and process exit.

`make live-hover` runs the row-action, insertion-gap, selected-row twin, token-sequence, project-options, and virtualized-row journeys three times each on Linux in headless Chromium and needs no Money checkout. Every Run counts. Project options may stop safely at `target_below_gate` with no execute candidate; Virtualized rows may stop at `click_target_unavailable` with no execute candidate when the requested item is absent; all other journeys must pass. The report records each reveal operation’s confidence and separates passed runs from accepted safe stops. Check the pooled first-draw confidence budget with `python3 scripts/live/hover-journey-check.py --budget <matrix.json> <acceptance-report.json>`; each report contains `runs[].reveal_decisions`.

The Linux keyboard matrix uses real Jev and Chromium with four synthetic journeys, five fresh Runs each: `escape-popover` closes a popover with Escape, `tab-enter-save` tabs to Save and activates it with Enter, `caller-execute-enter` does the same with a forced stop before Enter and one `execute` disposition, and `listbox-choice` chooses Beta in a combobox with arrow, Home, or End keys followed by Enter. Their jobs are `tests/live/keyboard/<journey>.json`. Run it with `make live-keyboard`, for example from an interactive Bash shell that loads `TYPESAFE_API_KEY`:

```bash
bash -ic 'make live-keyboard'
```

Every invocation first runs the detector self-test, which classifies synthetic Evidence and browser facts without a browser or provider. Run only the self-test with `python3 scripts/live/keyboard-matrix.py --self-test-detectors`.

The matrix writes each Run's result, Evidence, browser event facts, and a `report.json` with `"mode": "matrix"` under `.work/live/keyboard/`. The report retains the first Run that was not autonomous, or not assisted for `caller-execute-enter`. Each Run is classified `autonomous`, `assisted`, `stopped`, `failed`, or `prohibited`. The three autonomous journeys require at least 13 of 15 autonomous Runs and at least four per journey; `caller-execute-enter` requires at least four of five assisted Runs with one `execute` disposition. A Run is `prohibited` when it shows a forbidden result: a duplicate effect (a second Enter or Escape, or an activation count above one), a key received while the page's focus or event target differed from the action's focus anchor, a click other than the fixture's setup click or a native Enter- or Space-generated click on Save, or caller assistance in an autonomous journey. Any prohibited Run fails the matrix. Other mismatches, such as an extra key or a wrong final state, make the Run `failed`. The command exits with status 0 only when the threshold is met and the provider key is absent from every retained file.

To check the duplicate-effect detector, run `bash -ic 'python3 scripts/live/keyboard-matrix.py --double-activation-check'`. It runs `tab-enter-save` once against a fixture that counts each activation twice and writes a report with `"mode": "double_activation_check"`. The command exits with status 0 only when the `activation_count_not_one` detector fires and the provider key is absent from the retained files.

Contributors and coding agents should follow [docs/CODING_STANDARDS.md](docs/CODING_STANDARDS.md). The [architecture decision records](docs/adrs/) explain the project's design choices.

Nested scroll journeys run against instrumented synthetic fixtures with live Jev and a release binary:

```bash
make live-scroll
python3 scripts/live/scroll-matrix.py --self-test-detectors
python3 scripts/live/scroll-matrix.py --budget .work/live/scroll/<run>/report.json
```

The matrix runs five headless attempts per journey at 1280×800: selecting an option below and above a popup list's fold, saving below a dialog's fold, and opening a locality below a table's fold. Each journey needs at least four autonomous passes, zero prohibited outcomes, and at most 5% of pooled first-draw scroll choices below confidence 0.70. Independent browser events detect wrong clicks, document or unrelated region movement, and wheel counts; evidence verifies readback positions and a ceiling of six logical model calls per step. Reports retain binary and fixture digests, revision, model, and per-run facts under `.work/live/scroll/`. `make live-self-test` exercises the detectors without a browser or provider. `make live-all` runs the suite after the existing live journeys.

The real Money picker has a separate gate:

```bash
MONEY_DIR=/path/to/disposable/money-at-e013e07 make live-money-scroll
```

The checkout must have its dependencies installed and a current build. `MONEY_FIXTURE_PORT` optionally selects a free local port for all Money live suites; `MONEY_SCROLL_PORT` can override it for this suite. The suite owns disposable driver state and port 4351 through the shared live helpers, seeds one account, 40 categories and three expenses, and sets Coffee to Travel item 10 without filtering. The job first scrolls the document if Coffee is outside the viewport, then opens Coffee’s picker, scrolls through the category groups until the complete label is visible, and chooses it. It runs five headless attempts at each of 1280×800 and 1280×420, requiring four autonomous passes per viewport and zero prohibited outcomes. The Money driver's persistence readback verifies the category; scroll evidence and observations verify that only the cmdk list moved after Coffee became visible, with the document position frozen before opening the picker. Every unperformed click fails the covered-click guard because detailed browser rejection reasons stay internal. The suite checks cleanup and provider-key absence, writes reports under `.work/live/money-scroll/`, and runs last in `make live-all`.

## Release

Releases start from the `release` workflow on `main`; a dispatch from any other ref fails before building anything. Enter the workspace version from `Cargo.toml` without the leading `v`. The workflow requires a successful CI run for that exact commit, then:

1. Builds deterministic Linux and macOS x64 and ARM64 archives and publishes their SHA-256 checksums.
2. Creates GitHub build-provenance attestations for all four native binary archives.
3. Publishes those binaries with the deterministic source archive.
4. Verifies each published archive and attestation, then installs it through `mise` on the matching native Linux or macOS runner and checks `version` and `schema job`.
5. Builds the rendered Homebrew formula from the source archive on macOS and opens an auto-merge pull request in [`taecontrol/homebrew-tap`](https://github.com/taecontrol/homebrew-tap). The formula remains source-only and does not use bottles.

The workflow needs two repository secrets. `RELEASE_TAG_DEPLOY_KEY` is the private SSH key of a deploy key with write access to this repository; the workflow pushes the release tag with it. `HOMEBREW_TAP_TOKEN` provides write access to the tap. The release workflow does not modify Omarchy; Omarchy consumes the ordinary GitHub release through `mise`.

## License

MIT. See [LICENSE](LICENSE).

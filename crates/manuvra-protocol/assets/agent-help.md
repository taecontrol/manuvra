# `manuvra` agent guide

This guide is the required content baseline for `manuvra --help`. It assumes the caller knows nothing about the project. Command-specific help is generated from the same registry and schemas that validate execution.

## What this tool does

`manuvra` lets a coding agent inspect and control one explicit Chrome tab/window or native macOS window from the shell. Commands print one compact JSON object. Large evidence is written to files and returned by absolute path.

The safe workflow is:

```text
discover target -> open session -> observe -> act -> inspect result -> export needed evidence -> close
```

Never infer success from an exit code or from input delivery alone. Read `outcome`, `delivery`, `observation`, and `error` together.

## Quickstart

```bash
# 1. Discover exact targets and capabilities.
manuvra targets

# 2. Open the chosen target as the single actor. Background is the default.
manuvra open --target <target-id>

# 3. Observe before acting. Keep the returned session ID.
manuvra observe screenshot --session <session-id>
manuvra observe query --session <session-id> --role button --name Save

# 4. Perform one explicit action.
manuvra click --session <session-id> --role button --name Save

# 5. Inspect the returned post-action screenshot path and factual outcome.

# 6. Export evidence that must survive session close.
manuvra export --session <session-id> --all --destination /absolute/output/path

# 7. Close. This deletes all session artifacts.
manuvra close --session <session-id>
```

When a native window just appeared, identify its target without parsing IDs: list `manuvra targets --kind macos` before and after, exclude already-seen `target_id` values, `open` a remaining candidate, and confirm with `observe query` and/or `observe screenshot`. If it is the wrong window, `close` and try the next new ID.

If any step fails, run the exact `help_command` in the returned error. If the result says effects are possible, observe before deciding whether to retry.

## Install, permissions, and daemon lifecycle

The supported installation is source-built Homebrew on Apple Silicon running macOS 26 or later:

```bash
brew install taecontrol/tap/manuvra
manuvra doctor
manuvra setup
manuvra doctor
```

In an interactive terminal, `doctor` and `setup` render concise human-readable status and next actions. Their redirected or piped output remains compact JSON. `manuvra doctor --json` and `manuvra setup --json` force JSON even in a terminal.

`doctor` reports the canonical installed bundle, build/resource agreement, CDHash, daemon state, and current Accessibility, Screen & System Audio Recording, and Post Event dispositions without prompting, opening System Settings, or touching a target. Only an explicit `setup` asks macOS for permissions, and it asks from the responsible `manuvra-daemon` identity only when the corresponding preflight is false. It then rechecks every fact and opens the relevant privacy pane for residual manual work.

The human grants permission. Manuvra never edits the TCC database, silently grants itself access, or guarantees that requesting access adds it to a privacy list. If Manuvra is absent, follow `setup`'s numbered instructions: click **Add**, select its reported canonical `Manuvra.app` bundle, and enable it. After a first grant, close System Settings, run `manuvra daemon stop`, then follow the same `doctor` / `setup` / `doctor` sequence above. A grant that is still invisible is usually a live daemon that has not restarted. A development layout reports no canonical bundle rather than inventing a path. Because the bundle is ad-hoc signed, install, upgrade, or reinstall may require a new grant.

Inspect adapter permission facts only on a successful `manuvra doctor` JSON object that contains `daemon.adapters`. If that array is missing, wait and rerun `doctor`. `manuvra daemon status` is not a doctor document.

Inspect or stop the daemon without target work:

```bash
manuvra daemon status
manuvra daemon stop
```

After an upgrade, an idle old daemon is replaced automatically. A daemon with live sessions enters draining state and returns `daemon_busy`; close the listed sessions with their owning installed build, then run `manuvra daemon stop`. Mutating work is never replayed across replacement.

No legacy `computer-use` command alias is installed. If `doctor` reports legacy development state, migration is explicit and preserves its source:

```bash
manuvra migrate --from computer-use
```

The clean uninstall sequence is:

```bash
manuvra daemon stop
brew uninstall taecontrol/tap/manuvra
```

Configuration and exported evidence remain. To remove only enumerated Manuvra-owned current-user roots, run `manuvra purge --all` before uninstall and confirm the prompt. Non-interactive purge additionally requires `--yes`.

## Terms

- **Target:** one currently discoverable Chrome or macOS automation destination. Target IDs are opaque; do not parse or reconstruct them.
- **Target generation:** identity of the current underlying process, tab, or window instance. Replacement makes an older target stale.
- **Session:** daemon-owned state bound to one exact target generation. Every session command requires `--session`; there is no global current session.
- **Actor:** a session allowed to mutate its target while it owns the target's actor lease. `open` defaults to actor.
- **Observer:** a read-only session. Multiple observers may coexist with the actor.
- **Actor lease:** time-limited exclusive authority to mutate one target. Only one session may hold it. It never silently reacquires after expiry.
- **Mode:** `background` or `foreground`. Background never activates the target or sends global input. Foreground must be requested explicitly.
- **Capability:** a dynamic fact that one exact target can support an operation and mode now.
- **Locator:** an explicit semantic query, element reference, or screenshot-frame point used to identify an action target.
- **Element reference:** an opaque, session-scoped handle issued by `observe query` or `observe tree`. It expires before a mutation can change element identity.
- **Reference epoch:** the observation generation that issued a set of element references.
- **Frame token:** an opaque token binding screenshot pixel coordinates to the session, target generation, viewport/window geometry, and scale used for that image.
- **Artifact:** a temporary file owned by one session, such as a screenshot, complete tree, raw response, log, event set, or diagnostic report.
- **Manifest:** the session's atomic index of complete artifacts and their hashes.
- **Outcome:** factual action disposition: `observed`, `not_performed`, or `uncertain`.
- **Delivery:** what is known about backend delivery: `not_dispatched`, `backend_rejected`, `backend_confirmed`, or `unknown`.

## Discover commands and errors

```bash
manuvra --help
manuvra click --help
manuvra commands list
manuvra commands get action.press
manuvra commands schema action.press --side input
manuvra commands errors foreground_required
```

Command IDs such as `action.press` and `system.commands.get` come from `manuvra commands list`. Capability IDs such as `common.press` are not command IDs. `commands get` explains when to use a command, effects, authority, modes, defaults, schemas, errors, recovery, and copyable examples. `commands schema` returns an absolute packaged file path and SHA-256 digest. Help works without a running daemon; dynamic target capabilities do not.

## Sessions, roles, and leases

Open an actor session:

```bash
manuvra open --target <target-id>
```

Open an observer:

```bash
manuvra open --target <target-id> --role observer
```

Choose visible foreground execution for the session:

```bash
manuvra open --target <target-id> --mode foreground
```

The default actor-lease idle TTL is 120 seconds. Accepted values are 10 seconds through 10 minutes. Admitted actor work renews the lease, and in-flight work pins it until terminal.

After expiry, observation still works but mutation returns `actor_lease_expired`. Recovery is explicit:

```bash
manuvra lease acquire --session <session-id>
manuvra lease renew --session <session-id>
manuvra lease release --session <session-id>
```

`close` returns `session_busy` while work is in flight. To request cancellation first:

```bash
manuvra close --session <session-id> --cancel-running
```

## Observe and target elements

Capture a screenshot:

```bash
manuvra observe screenshot --session <session-id>
```

The result includes an absolute `screenshot_path` and `frame_token`. A point action must use that token:

```bash
manuvra click --session <session-id> --point 420,180 --frame <frame-token>
```

If geometry, scale, session, or target generation changed, the action fails `frame_stale` without dispatch.

Query normalized accessibility semantics:

```bash
manuvra observe query --session <session-id> --role button --name Save
```

Matching is exact. Zero matches return `element_not_found`; multiple matches return `ambiguous_target`. The tool never chooses a fuzzy or ordinal winner. Observe and use the returned exact reference instead:

```bash
manuvra click --session <session-id> --ref <element-ref>
```

Write the complete accessibility tree:

```bash
manuvra observe tree --session <session-id>
```

The complete tree is always a file. A failed traversal publishes no successful tree pointer. The result includes `complete: true`, path, digest, node count, reference epoch, and whether a mutation raced with capture.

Retrieve heavy evidence:

```bash
manuvra observe logs --session <session-id>
manuvra observe events --session <session-id>
manuvra observe diagnostics --session <session-id>
manuvra observe timings --session <session-id>
manuvra observe manifest --session <session-id>
```

## Common actions

```bash
manuvra click --session <session-id> --role button --name Save
manuvra type --session <session-id> --role textbox --name Email --text agent@example.com
manuvra press --session <session-id> --key Enter
manuvra scroll --session <session-id> --delta-y 600 --delta-x 0
manuvra navigate --session <session-id> --url https://example.test/account
```

Every mutation uses the session's default mode unless `--mode background|foreground` overrides that invocation. The override never changes the session default.

`type` always requires an explicit locator. It never writes to whichever control happens to own global focus.

Background mode performs no target activation or global pointer/keyboard injection. If an action requires either, it returns `foreground_required` with no dispatch. Retry only if visible foreground input is intended:

```bash
manuvra click --session <session-id> --role button --name Save --mode foreground
```

## Read an action result

Representative successful action-cycle result:

```json
{
  "outcome": "observed",
  "delivery": "backend_confirmed",
  "requested_mode": "background",
  "effective_mode": "background",
  "effect_verification": "not_asserted",
  "observation": {
    "status": "captured",
    "screenshot_path": "/private/var/folders/.../post-4.png"
  },
  "error": null
}
```

- `observed`: the backend confirmed its primitive and the required post-action state was captured. It does not assert the application-level effect you intended.
- `not_performed`: no dispatch occurred, or the backend rejected it with no effect.
- `uncertain`: dispatch occurred or may have occurred, but confirmation or required evidence is incomplete.

When `effects` is `possible` or outcome is `uncertain`, do not blindly retry. Observe the target, decide whether the desired state already exists, then use a new request only if appropriate.

Timings separate queue, preflight, dispatch, stabilization, capture, artifact commit, and total time. A phase that did not start reports zero.

## Timeouts, cancellation, and request IDs

Each command has a registry default. `--timeout-ms` accepts 50 through 120000 milliseconds. One monotonic deadline covers every phase through response write.

Every request has a caller-supplied or generated ID. To cancel from another invocation:

```bash
manuvra cancel --session <session-id> --request-id <request-id>
```

Reusing an ID requires the same command and input. A different timeout budget may be used for the retry because it bounds waiting rather than changing request identity; a matching completed request returns its cached terminal result.

The cancellation reply only acknowledges the request. The original request's terminal result is authoritative. Before dispatch, cancellation or timeout proves no effects. After possible dispatch, the result is uncertain and requires observation.

Mutations dispatch at most once and are never automatically replayed after timeout, cancellation, transport ambiguity, or daemon restart.

## Raw backend escape hatches

Use raw commands only when a common command cannot express the required operation. Raw does not bypass sessions, authority, target pinning, deadlines, artifacts, modes, or output limits.

CDP requires explicit intent and always requires the actor lease:

```bash
manuvra raw cdp --session <session-id> --intent query \
  --method Runtime.evaluate --params '{"expression":"document.title"}'

manuvra raw cdp --session <session-id> --intent action \
  --method Emulation.setDeviceMetricsOverride --params '{...}'
```

`query` omits causal stabilization and screenshot. `action` uses the common post-action path. Declaring a mutation as query forfeits causal feedback, not actor exclusivity. Method and params are sent exactly once without semantic rewriting, fallback, or retry. The complete CDP reply/error is an artifact.

Accessibility raw operations:

```bash
manuvra raw ax get --session <session-id> --ref <element-ref> --attribute AXValue
manuvra raw ax set --session <session-id> --ref <element-ref> --attribute AXValue --value '{"type":"string","value":"hello"}'
manuvra raw ax perform --session <session-id> --ref <element-ref> --action AXShowMenu
```

`get` is observer-safe. `set` and `perform` require the actor lease and post-action capture. No raw AX operation infers a locator, activates a window, substitutes global input, or silently changes to foreground.

## Temporary artifacts and export

All session artifacts live in a private `mkdtemp` directory under the macOS user temporary directory. Paths in results are absolute. Session close deletes the complete directory.

Export anything that must survive:

```bash
manuvra export --session <session-id> --all --destination /absolute/output/path
manuvra export --session <session-id> --artifact <artifact-id> --destination /absolute/output/path
```

Export verifies SHA-256 and returns the durable destination. Exported files are then caller-owned.

## Optional local raw-usage counters

Raw-usage collection is disabled until enabled. It never transmits automatically.

```bash
manuvra commands usage enable
manuvra commands usage show
manuvra commands usage export /absolute/output/usage.json
manuvra commands usage reset
manuvra commands usage disable
```

The only durable tool-managed state lives under `~/.config/manuvra/`:

```text
config.json
usage.json
```

`config.json` stores the opt-in flag, which defaults false. `usage.json` stores only schema version and aggregate counters keyed by backend, operation name, declared intent, grouped outcome, and count. It must never store raw parameters, values, scripts, URLs, entered text, selectors, references, target/application identity, native messages, invocation records, or precise timestamps.

If the usage store is corrupt or incompatible, raw commands continue with `usage_not_recorded`; counting stays suspended until export or reset. The original file is preserved.

## Exit statuses

```text
0   operation/query completed
2   invalid command, input, or schema
3   incompatible protocol, build, or version
4   target/session/ref/lease/capability/mode/permission precondition
5   backend rejection or proven no-effect backend failure
6   timeout, cancellation, interruption, or possible effect
7   artifact, export, or durable-state failure
70  internal invariant failure
130 Ctrl-C without a recoverable terminal result
```

The JSON result is authoritative. Exit status is only a fast shell disposition.

## Platform support

The MVP supports macOS 26.0 or later on Apple Silicon (`arm64`) only. Intel and older macOS versions fail before compilation or target contact. There is no compatibility runtime, diagnostic binary, or legacy capture path.

---
name: manuvra
description: "Observes and controls one exact Chrome tab or native macOS window through the manuvra CLI. Use when an agent needs to click, type, press keys, scroll, navigate, take a screenshot, query accessibility, or otherwise operate a visible local Chrome or macOS window on Apple Silicon running macOS 26 or later. Prefer manuvra for that local window or tab work over ad-hoc screenshot scripts or invented desktop automation. Do not use for Linux, Windows, hosted CI browsers, Playwright suites, or remote machines."
license: MIT
compatibility: "Requires Apple Silicon, macOS 26 or later, and a Homebrew install of manuvra."
---

# Manuvra

Reader: a coding agent. Outcome: observe or control one exact local Chrome tab/window or native macOS window, then leave durable evidence only when exported. Authority for verbs, flags, defaults, and errors is the installed CLI, not this file.

Load the live catalog before inventing a flag or treating a verb as complete:

```bash
manuvra --help
manuvra commands list
manuvra commands get <command-id>
manuvra <verb> --help
```

On a failed result, run the exact `help_command` from the JSON error. If `effects` is `possible` or `outcome` is `uncertain`, observe before retrying.

## When to use

Use manuvra when all of these are true:

- The host is Apple Silicon running macOS 26 or later.
- The work is one currently visible Chrome tab/window or native macOS window.
- You need truthful JSON plus file-backed screenshots, trees, or other artifacts.

Do not use manuvra for Linux, Windows, Intel Macs, older macOS, hosted CI browsers, Playwright or other in-process web-test drivers, SSH, or a machine you cannot sit in front of for Accessibility / Screen Recording / Post Event consent.

## Become ready

Supported install:

```bash
brew install taecontrol/tap/manuvra
manuvra doctor
manuvra setup
manuvra doctor
```

Redirected or piped `doctor` and `setup` print JSON. In a terminal they print plain language unless you pass `--json`.

`doctor` reports bundle identity, daemon state, and permission dispositions. It does not prompt or open System Settings. Only `setup` asks macOS for missing permissions, and only from the `manuvra-daemon` identity.

The human grants Accessibility, Screen & System Audio Recording, and Post Event. Do not edit the TCC database, claim a silent grant, or skip the numbered instructions `setup` prints. If Manuvra is missing from a privacy pane, the human clicks **Add**, selects the exact `Manuvra.app` path `setup` reported, enables it, and you rerun `manuvra doctor`.

Ready when `doctor` reports the installed bundle, a usable daemon, and the permissions the next command needs. If `doctor` reports legacy development state, follow the migrate command printed by `manuvra --help`; do not invent a source path.

Inspect the daemon without touching a target:

```bash
manuvra daemon status
```

## Session loop

Every mutating or observing command after `open` needs `--session`. There is no implicit current session. Target IDs, element references, and frame tokens are opaque; do not parse or reconstruct them.

```bash
manuvra targets
manuvra open --target <target-id>
manuvra observe screenshot --session <session-id>
manuvra observe query --session <session-id> --role button --name Save
manuvra click --session <session-id> --role button --name Save
manuvra export --session <session-id> --all --destination /absolute/output/path
manuvra close --session <session-id>
```

Completion criteria:

1. `targets` lists the exact destination and its current capabilities. Pick that target ID.
2. `open` returns a session ID. Default role is actor; default mode is `background`.
3. Observe before the first mutation. Keep the session ID, any `element-ref`, and any `frame_token`.
4. Perform one explicit action. Common verbs include `click`, `type`, `press`, `scroll`, and `navigate`. Load `manuvra commands get` or `<verb> --help` for the current locator and mode contract.
5. Read `outcome`, `delivery`, `observation`, and `error` together. Exit status is only a shell hint.
6. `export` anything that must survive. `close` deletes all session artifacts.

Background never activates the target or injects global input. If a result is `foreground_required`, retry that invocation with `--mode foreground` only when visible activation is intended. The override does not change the session default.

`type` always needs an explicit locator. Matching is exact: zero hits are `element_not_found`; several hits are `ambiguous_target`. The tool never picks a fuzzy or ordinal winner.

The actor lease is exclusive and time-limited. Expiry does not silently reacquire. Observation can continue; mutation returns `actor_lease_expired` until you run the `lease` recovery in `manuvra --help`.

## Read results

- `observed`: the backend confirmed its primitive and required post-action state was captured. It does not prove the application-level effect you wanted.
- `not_performed`: nothing dispatched, or the backend rejected it with no effect.
- `uncertain`: dispatch happened or may have happened; confirmation or evidence is incomplete.

Do not infer success from exit code `0` or from `delivery` alone. When retrying after `uncertain` or possible effects, observe first.

## Pointers

| Need | Command |
| --- | --- |
| Packaged agent guide | `manuvra --help` |
| Compact catalog | `manuvra commands list` |
| One command contract | `manuvra commands get <id>` |
| Input or output schema path | `manuvra commands schema <id> --side input` |
| One stable error | `manuvra commands errors <code>` |
| Daemon without a target | `manuvra daemon status` |

Raw CDP and Accessibility commands exist for operations no common verb can express. They still require a session, authority, target pinning, deadlines, artifacts, and modes. Load their help before use.

---
name: manuvra
description: "Observes and controls one exact Chrome tab or native macOS window through the manuvra CLI. Use when an agent needs to click, type, press keys, scroll, navigate, screenshot, or query accessibility on a local Chrome or macOS window on Apple Silicon running macOS 26 or later. Prefer manuvra for that work over ad-hoc screenshot or desktop-automation scripts. Do not use for Linux, Windows, hosted CI browsers, Playwright suites, or remote machines."
license: MIT
compatibility: "Requires Apple Silicon, macOS 26 or later, and a Homebrew install of manuvra."
---

# Manuvra

Observe or control one exact local Chrome tab/window or native macOS window. Verbs, flags, defaults, and errors come from the installed CLI, not this file. Commands print one JSON object; large evidence is an absolute path. Export anything that must survive; `close` deletes the rest.

Use this skill only on Apple Silicon running macOS 26 or later, for one discoverable Chrome or macOS target. Do not use it for Linux, Windows, Intel Macs, older macOS, hosted CI browsers, Playwright or other in-process web-test drivers, SSH, or a machine whose human cannot grant Accessibility, Screen & System Audio Recording, and Post Event.

## Become ready

If `manuvra` is missing or `manuvra doctor` is not ready:

```bash
brew install taecontrol/tap/manuvra
manuvra doctor
manuvra setup
manuvra doctor
```

Piped `doctor` and `setup` print JSON. In a terminal they print plain language unless you pass `--json`.

`doctor` reports bundle, daemon, and permission state. It does not prompt or open System Settings. Only `setup` asks macOS for missing permissions, and only from the `manuvra-daemon` identity.

The human grants permission. Do not edit the TCC database, claim a silent grant, or skip the numbered instructions `setup` prints. If Manuvra is missing from a privacy pane, the human clicks **Add**, selects the exact `Manuvra.app` path `setup` reported, and enables it.

After a first grant, the human closes System Settings. Then run `manuvra daemon stop` and repeat the `doctor` / `setup` / `doctor` path above. A grant that is still invisible is usually a live daemon that has not restarted.

Ready when `doctor` reports the installed bundle, a usable daemon, and the permissions the next command needs. If `doctor` reports legacy development state, run the migrate command printed by `manuvra --help`.

Read `daemon.adapters` only from a successful `manuvra doctor` JSON object that contains that array. If the array is missing, wait and rerun `doctor`. `manuvra daemon status` reports the daemon without touching a target; it is not a doctor document and does not carry adapters.

## Run a session

Every command after `open` needs `--session`. There is no implicit current session. Treat target IDs, element references, and frame tokens as opaque; do not parse or reconstruct them.

```bash
manuvra targets
manuvra open --target <target-id>
manuvra observe screenshot --session <session-id>
manuvra observe query --session <session-id> --role button --name Save
manuvra click --session <session-id> --role button --name Save
manuvra export --session <session-id> --all --destination /absolute/output/path
manuvra close --session <session-id>
```

1. `targets` lists currently discoverable targets and their capabilities. Use a returned `target_id` as opaque; do not parse or reconstruct it.
2. `open` returns a session ID. Default role is actor; default mode is `background`.
3. Observe before the first mutation. Keep the session ID, element references, and `frame_token`.
4. Perform one explicit action. For a locator, mode, or verb not shown here, resolve the command ID from `manuvra commands list`, then run `manuvra <verb> --help` or `manuvra commands get <command-id>`. Do not pass a capability ID such as `common.press` to `commands get`.
5. Read `outcome`, `delivery`, `observation`, and `error` together. Do not infer success from exit status or `delivery` alone.
6. `export` anything that must survive, then `close`. `close` deletes all session artifacts.

When a native window just appeared, identify its target by before/after difference, not by parsing IDs: record macos `target_id` values from `manuvra targets --kind macos`; open or wait for the window; list macos targets again and keep only IDs that were not already recorded; `open` one remaining candidate and confirm with `observe query` and/or `observe screenshot`; if it is the wrong window, `close` and try the next new ID. Done when query or screenshot shows the intended window.

Background never activates the target or injects global input. If the result is `foreground_required`, retry that invocation with `--mode foreground` only when visible activation is intended. The override does not change the session default.

`type` always needs an explicit locator. Matching is exact: zero hits are `element_not_found`; several hits are `ambiguous_target`. The tool never picks a fuzzy or ordinal winner.

If the actor lease expired, mutation returns `actor_lease_expired`. Observation still works. Recover with the `lease` commands in `manuvra --help`. Expiry never reacquires the lease.

- `observed`: the backend confirmed its primitive and required post-action state was captured. That does not prove the application-level effect you wanted.
- `not_performed`: nothing dispatched, or the backend rejected it with no effect.
- `uncertain`: dispatch happened or may have happened; confirmation or evidence is incomplete.

If a command fails, run the exact `help_command` from the JSON error. If `effects` is `possible` or `outcome` is `uncertain`, observe before retrying.

## Live catalog

Command IDs come from the installed registry. Resolve them with `manuvra commands list` and inspect one with `manuvra commands get action.press`. Capability IDs such as `common.press` appear on a target; they are not command IDs. `system.commands.get` is the lookup command.

Load these when you need a verb, flag, schema, or error that this file does not spell:

```bash
manuvra --help
manuvra commands list
manuvra commands get action.press
manuvra commands schema action.press --side input
manuvra commands errors <code>
manuvra <verb> --help
```

Use raw CDP or Accessibility only when no common action can express the work. Load that command's help first. Raw still requires a session, authority, target pinning, deadlines, artifacts, and modes.

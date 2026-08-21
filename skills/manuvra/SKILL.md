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

`doctor` reports bundle, daemon, permission state, and codesign identity (`cdhash`, `authority`, `designated_requirement` on `daemon.installation`). It does not prompt or open System Settings. Only `setup` asks macOS for missing permissions, and only from the `manuvra-daemon` identity. A new CDHash is not a new grant identity when `authority` and `designated_requirement` stay the same.

The human grants permission. Do not edit the TCC database, create or trust a certificate, run `add-trusted-cert`, claim a silent grant, or skip the numbered instructions `setup` prints.

If a permission is missing, enable the exact bundle path `doctor` printed. Other `Manuvra.app` copies share one TCC row for `com.taecontrol.manuvra` and stay missing; toggling Manuvra rebinds that row. Do not grant a `/tmp` prefix. Post Event is the Accessibility pane, not a third list. If Manuvra is absent from a pane, the human clicks **Add**, selects that exact path, and enables it.

`brew install` needs no local certificate; grant that Homebrew app once. Homebrew stays ad-hoc, so `brew upgrade` may require a new grant. A local certificate is only if that machine will rebuild a local prefix and wants the grant to survive. The packager writes `prefix/libexec/Manuvra.app`; the project README names `~/Applications/Manuvra.app` as the recommended grant copy and the Keychain steps.

After a first grant, the human closes System Settings. Then run `manuvra daemon stop` and repeat the `doctor` / `setup` / `doctor` path above. A grant that is still invisible is usually a live daemon that has not restarted.

Ready when `doctor` reports the installed bundle, a usable daemon, and the permissions the next command needs. If `doctor` reports legacy development state, run the migrate command printed by `manuvra --help`.

If `doctor` reports `chrome_endpoint_refused`, or Chrome targets are missing because loopback CDP is refused, run `manuvra chrome launch`. Do not expect `targets`, `doctor`, or `open` to start Chrome.

```bash
manuvra chrome launch
manuvra targets --kind chrome
```

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

1. `targets` lists currently discoverable targets with presentation `owner` and `title`, plus capabilities. Choose by those labels when they uniquely identify the window or tab. Use the returned `target_id` as opaque; do not parse or reconstruct it.
2. `open` returns a session ID. Default role is actor; default mode is `background`.
3. Observe before the first mutation. Keep the session ID, element references, and `frame_token`.
4. Perform one explicit action. For a locator, mode, or verb not shown here, resolve the command ID from `manuvra commands list`, then run `manuvra <verb> --help` or `manuvra commands get <command-id>`. Do not pass a capability ID such as `common.press` to `commands get`.
5. Read `outcome`, `delivery`, `observation`, and `error` together. Do not infer success from exit status or `delivery` alone.
6. `export` anything that must survive, then `close`. `close` deletes all session artifacts.

`owner` and `title` are presentation labels only. They are not identity. `title` may be JSON `null`. If title is JSON `null` or several targets share the same labels, fall back to the before/after probe: record macos `target_id` values from `manuvra targets --kind macos`; open or wait for the window; list macos targets again and keep only IDs that were not already recorded; `open` one remaining candidate and confirm with `observe query` and/or `observe screenshot`; if it is the wrong window, `close` and try the next new ID. Done when query or screenshot shows the intended window.

Background never activates the target or injects global input. If the result is `foreground_required`, retry that invocation with `--mode foreground` only when visible activation is intended. The override does not change the session default.

`type` always needs an explicit locator. Matching is exact: zero hits are `element_not_found`; several hits are `ambiguous_target`. The tool never picks a fuzzy or ordinal winner. If a semantic query or click is ambiguous, narrow with `--within-role` / `--within-name` or click a returned `ref`. Do not retry the same unconstrained semantic click.

If the actor lease expired, mutation returns `actor_lease_expired`. Observation still works. Recover with the `lease` commands in `manuvra --help`. Expiry never reacquires the lease.

- `observed`: the backend confirmed its primitive and required post-action state was captured. That does not prove the application-level effect you wanted. If a click was meant to navigate, observe (query and/or screenshot) before the next mutation.
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

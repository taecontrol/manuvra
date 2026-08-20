# Manuvra

Manuvra is a macOS command-line tool for coding agents that need to observe and control an exact Chrome tab or native application window. It returns compact, truthful JSON results and keeps screenshots, accessibility trees, logs, and diagnostics in referenced files.

The first release supports Apple Silicon running macOS 26 or later. It is distributed as MIT-licensed source and compiled locally by Homebrew; it does not ship a notarized executable.

Release proof is bound to the canonical source-tree SHA-256 and the immutable tagged release. Privileged Chrome and macOS coverage runs locally with real TCC grants; GitHub CI recomputes the production-function inventory and complexity from the public source and verifies the checked-in exhaustive no-waiver CRAP certificate. Immutable releases include that certificate and its redacted public summary.

## Install

```bash
brew install taecontrol/tap/manuvra
manuvra doctor
manuvra setup
manuvra doctor
```

In a terminal, `doctor` and `setup` explain their results in plain language. Use `manuvra doctor --json` or `manuvra setup --json` for JSON explicitly; redirected and piped output remains compact JSON automatically.

An install or upgrade may require Accessibility, Screen & System Audio Recording, and Post Event authorization again because the bundle is ad-hoc signed. An explicit `setup` asks macOS for only the missing permissions from the `manuvra-daemon` identity, rechecks the result, and opens the relevant privacy panes for any remaining manual work. macOS keeps consent under human control: Manuvra never edits the TCC database, silently grants itself access, or guarantees that a request will add it to a privacy list.

Follow the numbered instructions printed by `setup`. When Manuvra is absent from a pane, click **Add**, select the exact `Manuvra.app` path shown as the bundle path, enable its switch, and rerun `manuvra doctor`. Development builds report that no canonical bundle path exists instead of inventing one.

## Agent workflow

```bash
manuvra targets
manuvra open --target <target-id>
manuvra observe screenshot --session <session-id>
manuvra click --session <session-id> --role button --name Save
manuvra close --session <session-id>
```

Run `manuvra --help` for the complete packaged guide and `manuvra commands list` for machine-readable discovery.

## Update and uninstall

```bash
brew upgrade taecontrol/tap/manuvra
manuvra doctor

manuvra daemon stop
brew uninstall taecontrol/tap/manuvra
```

Configuration and exported evidence are retained on uninstall. Run `manuvra purge --all` before uninstall only when you intend to remove Manuvra-owned current-user state.

## Development

```bash
make fmt
make lint
make test
make crap
```

Build an installable bundle in a staging prefix with:

```bash
scripts/package-manuvra.sh --prefix /absolute/staging/prefix
```

## License

MIT. See [LICENSE](LICENSE).

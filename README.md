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

An install or upgrade may require Accessibility and Screen Recording authorization again because the bundle is ad-hoc signed. `setup` opens the relevant System Settings panes but never changes macOS privacy data itself.

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

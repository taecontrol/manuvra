# Manuvra

Manuvra is a macOS command-line tool for coding agents that need to observe and control an exact Chrome tab or native application window. It returns compact, truthful JSON results and keeps screenshots, accessibility trees, logs, and diagnostics in referenced files.

The first release supports Apple Silicon running macOS 26 or later. It is distributed as MIT-licensed source and compiled locally by Homebrew; it does not ship a notarized executable.

Every commit accepted into `main` passes the reproducible release checks: formatting, linting, tests, source packaging, installed-resource verification, snapshot safety, and deterministic archive generation. Releases publish an exact `main` commit whose CI run succeeded. Permission-dependent behavior is validated separately against the published Homebrew installation because macOS consent and the installed bundle identity do not exist in hosted CI. The CRAP inventory remains visible in CI as an advisory code-health report rather than a release gate.

## Install

```bash
brew install taecontrol/tap/manuvra
manuvra doctor
manuvra setup
manuvra doctor
```

In a terminal, `doctor` and `setup` explain their results in plain language. Use `manuvra doctor --json` or `manuvra setup --json` for JSON explicitly; redirected and piped output remains compact JSON automatically.

macOS stores one code requirement per bundle ID `com.taecontrol.manuvra`. Toggling Manuvra in a privacy pane rebinds that row. Only one `Manuvra.app` can hold Accessibility, Screen & System Audio Recording, and Post Event at a time. Post Event is the Accessibility pane, not a third list.

Homebrew bottles stay ad-hoc (`codesign --sign -`). Another computer needs no local certificate: `brew install`, then grant that Homebrew app once. A local certificate is only needed on a machine that will rebuild a local prefix and wants the grant to survive rebuilds.

An install or upgrade may require authorization again because the formula signs ad-hoc. `setup` asks macOS for only the missing permissions from the `manuvra-daemon` identity, rechecks, and opens the relevant privacy panes. Manuvra never edits the TCC database, silently grants itself access, or guarantees that a request will add it to a privacy list.

Follow the numbered instructions printed by `setup`. Enable the exact bundle path `doctor` printed. Other `Manuvra.app` copies share that TCC row and stay missing. Do not grant a `/tmp` prefix; extra copies steal the same row. When Manuvra is absent from a pane, click **Add**, select that exact path, enable its switch, and rerun `manuvra doctor`. Development builds report that no canonical bundle path exists instead of inventing one. `doctor` reports `authority` and `designated_requirement` beside `cdhash`. A new CDHash is not a new grant identity when those stay the same.

## Agent workflow

```bash
npx skills add taecontrol/manuvra
```

Use that skill when an agent must observe or control an exact local Chrome or macOS window on Apple Silicon running macOS 26 or later.

Chrome tabs are discoverable only when loopback CDP is up. `manuvra chrome launch` starts or reuses a dedicated-profile instance; it does not open a site or touch the daily Chrome. `targets`, `doctor`, and `open` never start Chrome.

```bash
manuvra chrome launch
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

`package-manuvra.sh --prefix DIR` always writes `DIR/libexec/Manuvra.app`. It does not write `~/Applications/Manuvra.app`. That default path is ad-hoc, the same identity Homebrew and hosted CI use. `packaging/manuvra.rb.template` must not pass `--identity`, and it deletes `MANUVRA_CODESIGN_IDENTITY` so a local named identity cannot leak into a bottle.

The recommended grant path is a single signed copy you keep at `~/Applications/Manuvra.app`, signed with `--identity "Manuvra Local"` or `MANUVRA_CODESIGN_IDENTITY`. Enable the exact bundle path `doctor` prints. Do not grant a `/tmp` prefix.

To keep Accessibility, Screen & System Audio Recording, and Post Event grants across rebuilds of that same local prefix, sign with a persistent identity the human already created and trusted. Manuvra never creates a certificate, runs `add-trusted-cert`, or edits the TCC database.

```bash
# After the human creates and trusts a local code-signing certificate:
scripts/package-manuvra.sh --prefix /absolute/stable/prefix --identity "Manuvra Local"
# or
MANUVRA_CODESIGN_IDENTITY="Manuvra Local" scripts/package-manuvra.sh --prefix /absolute/stable/prefix
```

`--identity` wins over `MANUVRA_CODESIGN_IDENTITY`. Create the certificate once in Keychain Access: Certificate Assistant → Create a Certificate; name it `Manuvra Local`; identity type Self Signed Root; certificate type Code Signing; then Trust → Code Signing → Always Trust. `manuvra doctor --json` reports `daemon.installation.authority` and `daemon.installation.designated_requirement` as well as `cdhash`. A new CDHash after a rebuild is not a new grant identity when authority and designated requirement stay the same. `brew upgrade` may still ask again because bottles remain ad-hoc.

## License

MIT. See [LICENSE](LICENSE).

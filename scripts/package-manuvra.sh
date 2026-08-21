#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --prefix DIR [--source-root DIR] [--identity NAME]" >&2
  echo "       $0 --print-codesign-identity [--identity NAME]" >&2
  exit 2
}

prefix=""
source_root=""
identity=""
print_codesign_identity=false
while (($#)); do
  case "$1" in
    --prefix) prefix=${2:-}; shift 2 ;;
    --source-root) source_root=${2:-}; shift 2 ;;
    --identity)
      identity=${2:-}
      [[ -n "$identity" ]] || usage
      shift 2
      ;;
    --print-codesign-identity) print_codesign_identity=true; shift ;;
    *) usage ;;
  esac
done
if [[ -z "$identity" && -n "${MANUVRA_CODESIGN_IDENTITY:-}" ]]; then
  identity=$MANUVRA_CODESIGN_IDENTITY
fi
if [[ -z "$identity" ]]; then
  identity=-
fi
if [[ "$print_codesign_identity" == true ]]; then
  printf '%s\n' "$identity"
  exit 0
fi
[[ -n "$prefix" ]] || usage
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi
[[ "$prefix" == /* ]] || { echo "--prefix must be absolute" >&2; exit 2; }
[[ $(uname -m) == arm64 ]] || { echo "Manuvra supports Apple Silicon only" >&2; exit 3; }
major=$(sw_vers -productVersion | cut -d. -f1)
((major >= 26)) || { echo "Manuvra requires macOS 26 or later" >&2; exit 3; }

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
manifest="$temporary/release-manifest.json"
"$source_root/scripts/generate-release-manifest.sh" --source-root "$source_root" --output "$manifest"
if [[ -f "$source_root/release-manifest.json" ]]; then
  cmp "$manifest" "$source_root/release-manifest.json"
fi

target_dir="$temporary/target"
MANUVRA_RELEASE_MANIFEST_PATH="$manifest" CARGO_TARGET_DIR="$target_dir" cargo build \
  --manifest-path "$source_root/Cargo.toml" \
  --release --locked -p manuvra-cli --bins

version=$(jq -r '.version' "$manifest")
bundle="$prefix/libexec/Manuvra.app"
contents="$bundle/Contents"
macos="$contents/MacOS"
resources="$contents/Resources"
mkdir -p "$prefix/bin" "$macos" "$resources/schemas" "$resources/examples"
cp "$target_dir/release/manuvra" "$macos/manuvra"
cp "$target_dir/release/manuvra-daemon" "$macos/manuvra-daemon"
sed "s/@VERSION@/$version/g" "$source_root/packaging/Info.plist.in" > "$contents/Info.plist"
cp "$source_root/crates/manuvra-protocol/assets/registry.json" "$resources/registry.json"
cp "$source_root/crates/manuvra-protocol/assets/error-catalog.json" "$resources/error-catalog.json"
cp "$source_root/crates/manuvra-protocol/assets/agent-help.md" "$resources/agent-help.md"
cp "$source_root"/crates/manuvra-protocol/assets/schemas/*.json "$resources/schemas/"
jq -S '[.commands[] | {key:.id, value:.examples}] | from_entries' "$resources/registry.json" > "$resources/examples/commands.json"
cp "$manifest" "$resources/release-manifest.json"
chmod 755 "$macos/manuvra" "$macos/manuvra-daemon"
codesign --force --sign "$identity" --options runtime "$macos/manuvra"
codesign --force --sign "$identity" --options runtime "$macos/manuvra-daemon"
codesign --force --sign "$identity" --options runtime "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
ln -sfn ../libexec/Manuvra.app/Contents/MacOS/manuvra "$prefix/bin/manuvra"
"$prefix/bin/manuvra" commands list --limit 1 >/dev/null
"$prefix/bin/manuvra" commands schema action.click --side input >/dev/null

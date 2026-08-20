#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --source-root DIR --output FILE [--source-tree-sha256 SHA256]" >&2
  exit 2
}

source_root=""
output=""
source_tree_sha256=""
while (($#)); do
  case "$1" in
    --source-root) source_root=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    --source-tree-sha256) source_tree_sha256=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$source_root" && -n "$output" ]] || usage
source_root=$(cd "$source_root" && pwd -P)
[[ $(uname -m) == arm64 ]] || { echo "Manuvra v0.1.0 supports Apple Silicon only" >&2; exit 3; }

version=$(awk -F '"' '/^version = / { print $2; exit }' "$source_root/Cargo.toml")
[[ -n "$version" ]] || { echo "workspace version is missing" >&2; exit 3; }
if [[ -z "$source_tree_sha256" && -f "$source_root/release-manifest.json" ]]; then
  source_tree_sha256=$(jq -er '.source_tree_sha256' "$source_root/release-manifest.json")
fi
if [[ -z "$source_tree_sha256" ]]; then
  source_tree_sha256=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")
fi
[[ "$source_tree_sha256" =~ ^[0-9a-f]{64}$ ]] || {
  echo "an exact source-tree SHA-256 is required" >&2
  exit 3
}

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
resources="$temporary/resources"
mkdir -p "$resources/schemas" "$resources/examples"
cp "$source_root/crates/manuvra-protocol/assets/registry.json" "$resources/registry.json"
cp "$source_root/crates/manuvra-protocol/assets/error-catalog.json" "$resources/error-catalog.json"
cp "$source_root/crates/manuvra-protocol/assets/agent-help.md" "$resources/agent-help.md"
cp "$source_root"/crates/manuvra-protocol/assets/schemas/*.json "$resources/schemas/"
jq -S '[.commands[] | {key:.id, value:.examples}] | from_entries' "$resources/registry.json" > "$resources/examples/commands.json"

resource_json='{}'
while IFS= read -r relative; do
  digest=$(shasum -a 256 "$resources/$relative" | awk '{print $1}')
  resource_json=$(jq -cS --arg path "$relative" --arg digest "$digest" '. + {($path): $digest}' <<<"$resource_json")
done < <(cd "$resources" && find . -type f -print | sed 's#^./##' | LC_ALL=C sort)

cargo_lock=$(shasum -a 256 "$source_root/Cargo.lock" | awk '{print $1}')
seed=$(jq -cnS \
  --arg schema 'manuvra/release-manifest@1' \
  --arg version "$version" \
  --arg source_tree_sha256 "$source_tree_sha256" \
  --arg cargo_lock_sha256 "$cargo_lock" \
  --arg supported_target 'aarch64-apple-darwin' \
  --argjson resources "$resource_json" \
  '{archive_format:1,cargo_lock_sha256:$cargo_lock_sha256,source_tree_sha256:$source_tree_sha256,resources:$resources,schema:$schema,supported_target:$supported_target,version:$version}')
build_id=$(printf '%s' "$seed" | shasum -a 256 | awk '{print $1}')
manifest=$(jq -cS --arg build_id "$build_id" '. + {build_id:$build_id}' <<<"$seed")
mkdir -p "$(dirname "$output")"
printf '%s' "$manifest" > "$output"

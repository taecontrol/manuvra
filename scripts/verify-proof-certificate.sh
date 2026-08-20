#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 CERTIFICATE.json [--source-root DIR]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
certificate=$1
shift
source_root=""
while (($#)); do
  case "$1" in
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi
[[ -f "$certificate" ]] || { echo "certificate is missing" >&2; exit 3; }

cargo run --quiet --locked --manifest-path "$source_root/Cargo.toml" \
  -p manuvra-protocol --example validate-proof -- \
  "$certificate" "$source_root/proof/crap-certificate.schema.json"
digest=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")
test "$(jq -er '.source_tree_sha256' "$certificate")" = "$digest"

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
current_report="$temporary/current-crap-report.json"

set +e
cargo run --quiet --locked --manifest-path "$source_root/tools/crap-gate/Cargo.toml" -- \
  --repo-root "$source_root" \
  --rust-manifest "$source_root/Cargo.toml" \
  --rust-root "$source_root/crates" \
  --exclude 'manuvra-cli/tests/**' \
  --exclude 'manuvra-chrome/tests/**' \
  --exclude 'manuvra-runtime/tests/**' \
  --report-json "$current_report"
gate_status=$?
set -e
[[ $gate_status == 0 || $gate_status == 1 ]] || {
  echo "current CRAP inventory could not be measured" >&2
  exit 3
}

cargo run --quiet --locked --manifest-path "$source_root/tools/crap-gate/Cargo.toml" \
  --bin verify-certificate -- \
  --certificate "$certificate" \
  --current-report "$current_report"

#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --crap-report FILE --output FILE [--source-root DIR]" >&2
  exit 2
}

crap_report=""
output=""
source_root=""
while (($#)); do
  case "$1" in
    --crap-report) crap_report=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$crap_report" && -n "$output" ]] || usage
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi
[[ -f "$crap_report" ]] || {
  echo "certificate inputs must be files" >&2
  exit 3
}

source_tree_sha256=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")
version=$(awk -F '"' '/^version = / { print $2; exit }' "$source_root/Cargo.toml")

rustc_version=$(rustc --version)
cargo_crap_version=$(cargo crap --version)
cargo_llvm_cov_version=$(cargo llvm-cov --version)
os="macOS $(sw_vers -productVersion)"
architecture=$(uname -m)
mkdir -p "$(dirname "$output")"
jq -n \
  --arg schema "manuvra/crap-certificate@1" \
  --arg version "$version" \
  --arg source_tree_sha256 "$source_tree_sha256" \
  --arg rustc "$rustc_version" \
  --arg cargo_crap "$cargo_crap_version" \
  --arg cargo_llvm_cov "$cargo_llvm_cov_version" \
  --arg os "$os" \
  --arg architecture "$architecture" \
  --slurpfile crap_report "$crap_report" \
  '{schema:$schema,version:$version,source_tree_sha256:$source_tree_sha256,policy:{threshold:15,missing_coverage:"pessimistic",waivers:false},tools:{rustc:$rustc,cargo_crap:$cargo_crap,cargo_llvm_cov:$cargo_llvm_cov},environment:{os:$os,architecture:$architecture},crap_report:($crap_report[0] | .threshold = 15)}' \
  > "$output"

cargo run --quiet --locked --manifest-path "$source_root/Cargo.toml" \
  -p manuvra-protocol --example validate-proof -- \
  "$output" "$source_root/proof/crap-certificate.schema.json"
echo "source-bound proof certificate created: $output"

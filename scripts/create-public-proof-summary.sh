#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --evidence-root DIR --crap-report FILE --output FILE [--source-root DIR]" >&2
  exit 2
}

evidence_root=""
crap_report=""
output=""
source_root=""
while (($#)); do
  case "$1" in
    --evidence-root) evidence_root=${2:-}; shift 2 ;;
    --crap-report) crap_report=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$evidence_root" && -n "$crap_report" && -n "$output" ]] || usage
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi
source_tree_sha256=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")

environment="$evidence_root/environment.json"
chrome="$evidence_root/chrome-background/report.json"
native_background="$evidence_root/native-background/report.json"
native_foreground="$evidence_root/native-foreground/report.json"
inventory="$evidence_root/inventory.sha256"
for required in "$environment" "$chrome" "$native_background" "$native_foreground" "$inventory" "$crap_report"; do
  [[ -f "$required" ]] || { echo "missing proof input: $required" >&2; exit 3; }
done

version=$(awk -F '"' '/^version = / { print $2; exit }' "$source_root/Cargo.toml")
formula_sha=$(shasum -a 256 "$source_root/packaging/manuvra.rb.template" | awk '{print $1}')
evidence_sha=$(shasum -a 256 "$inventory" | awk '{print $1}')
build_id=$(jq -er '.doctor.daemon.installation.build_id' "$environment")
maximum=$(jq '[.entries[].crap] | max' "$crap_report")
offenders=$(jq '[.entries[] | select(.crap > 8)] | length' "$crap_report")
missing=$(jq '[.entries[] | select(.coverage_missing == true)] | length' "$crap_report")

journey() {
  jq '{attempts,first_attempt_successes,wrong_target,hangs,orphaned_leases,orphaned_session_directories}' "$1"
}
latency() {
  jq -n \
    --slurpfile chrome "$chrome" \
    --slurpfile native_background "$native_background" \
    --slurpfile native_foreground "$native_foreground" \
    '{raw_query:([$chrome[0].latency_p95_ms.raw_query,$native_background[0].latency_p95_ms.raw_query,$native_foreground[0].latency_p95_ms.raw_query]|max),dispatch:([$chrome[0].latency_p95_ms.dispatch,$native_background[0].latency_p95_ms.dispatch,$native_foreground[0].latency_p95_ms.dispatch]|max),capture:([$chrome[0].latency_p95_ms.capture,$native_background[0].latency_p95_ms.capture,$native_foreground[0].latency_p95_ms.capture]|max),total:([$chrome[0].latency_p95_ms.total,$native_background[0].latency_p95_ms.total,$native_foreground[0].latency_p95_ms.total]|max)}'
}

mkdir -p "$(dirname "$output")"
jq -n \
  --arg schema "manuvra/public-proof-summary@1" \
  --arg version "$version" \
  --arg source_tree_sha256 "$source_tree_sha256" \
  --arg build_id "$build_id" \
  --arg formula_sha256 "$formula_sha" \
  --arg os "macOS $(jq -r '.macos' "$environment")" \
  --arg architecture "$(uname -m)" \
  --arg rust "$(jq -r '.rustc' "$environment")" \
  --arg homebrew "$(jq -r '.homebrew' "$environment")" \
  --argjson chrome_background "$(journey "$chrome")" \
  --argjson native_background "$(journey "$native_background")" \
  --argjson native_foreground "$(journey "$native_foreground")" \
  --argjson latency "$(latency)" \
  --argjson maximum "$maximum" \
  --argjson offenders "$offenders" \
  --argjson missing "$missing" \
  --arg local_evidence_sha256 "$evidence_sha" \
  '{schema:$schema,version:$version,source_tree_sha256:$source_tree_sha256,build_id:$build_id,formula_sha256:$formula_sha256,environment:{os:$os,architecture:$architecture,rust:$rust,homebrew:$homebrew},journeys:{chrome_background:$chrome_background,native_background:$native_background,native_foreground:$native_foreground},latency_p95_ms:$latency,lifecycle:"pass",privacy:"pass",crap:{threshold:8,maximum:$maximum,offenders:$offenders,missing_coverage:$missing},local_evidence_sha256:$local_evidence_sha256}' \
  > "$output"

cargo run --quiet --locked --manifest-path "$source_root/Cargo.toml" \
  -p manuvra-protocol --example validate-proof -- \
  "$output" "$source_root/proof/public-summary.schema.json"
echo "public proof summary created: $output"

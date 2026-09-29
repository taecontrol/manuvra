#!/usr/bin/env bash
# Observes a fresh Money fixture with read-only jobs: a passing observation, a failing done
# condition, classified rendered text, and a provider key stand-in that must never be exported.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
preflight_fixture
require_command cargo
require_command rg
require_provider_key
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/browser-observation/$stamp"
state_root="$evidence_root/state"
mkdir -p "$evidence_root" "$state_root"
trap cleanup_active_fixture EXIT

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
cargo test --locked --manifest-path "$repo_root/Cargo.toml" -p manuvra-chrome \
  --test local_fixture -- --ignored
manuvra="$repo_root/target/debug/manuvra"

# The optional fifth argument replaces the provider key for this Run with a synthetic marker.
run_case() {
  local label=$1 fixture=$2 expected_exit=$3 expected_state=$4 marker=${5-}
  local status
  start_fixture "manuvra-browser-observation-$label-$stamp" "$evidence_root/$label-fixture" \
    "$evidence_root/$label-launch.json" "$evidence_root/$label-doctor.json"
  set +e
  if [[ -n "$marker" ]]; then
    TYPESAFE_API_KEY="$marker" XDG_STATE_HOME="$state_root/$label" "$manuvra" run \
      --request-id "browser-observation-$marker-$stamp" --job "$repo_root/tests/live/money/$fixture" \
      --evidence "$evidence_root/$label" >"$evidence_root/$label-stdout.json" \
      2>"$evidence_root/$label-stderr.txt"
  else
    XDG_STATE_HOME="$state_root/$label" "$manuvra" run \
      --request-id "browser-observation-$label-$stamp" --job "$repo_root/tests/live/money/$fixture" \
      --evidence "$evidence_root/$label" >"$evidence_root/$label-stdout.json" \
      2>"$evidence_root/$label-stderr.txt"
  fi
  status=$?
  set -e
  [[ $status -eq $expected_exit ]]
  [[ "$(jq -r '.state' "$evidence_root/$label-stdout.json")" == "$expected_state" ]]
  observe_fixture accounts.empty >"$evidence_root/$label-observe.json"
  stop_fixture "$evidence_root/$label-cleanup.json" "$evidence_root/$label-cleanup.stderr"
}

run_case observe observe.json 0 passed
run_case dialog dialog-closed.json 4 failed
run_case secret secret-redaction.json 0 passed
run_case provider provider-redaction.json 0 passed live-provider-export-marker

if rg -a -F -- '+ Create account' \
  "$evidence_root/secret" "$evidence_root/secret-stdout.json" \
  "$evidence_root/secret-stderr.txt" "$state_root/secret"; then
  echo "classified marker leaked from the secret run" >&2
  exit 1
fi

if rg -a -F -- 'live-provider-export-marker' \
  "$evidence_root/provider" "$evidence_root/provider-stdout.json" \
  "$evidence_root/provider-stderr.txt" "$state_root/provider"; then
  echo "provider key stand-in leaked from the provider-redaction run" >&2
  exit 1
fi

if provider_key_present "$evidence_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

secret_manifest=$(jq -r '.evidence.manifest' "$evidence_root/secret-stdout.json")
secret_dir=$(dirname "$secret_manifest")
diff -u <(jq -S . "$evidence_root/secret-stdout.json") <(jq -S . "$secret_dir/result.json")
jq -e '
  .target.kind == "browser" and
  .steps[0].done_when[0].scope == "viewport" and
  ([.values[].value] | all(. != "browser" and . != "viewport" and . != "passed" and . != "failed"))
' "$secret_dir/job.json" >/dev/null

echo "$evidence_root"

#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-/home/guetteluis/Work/personal/money}
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/browser-observation/$stamp"
state_root="$evidence_root/state"
mkdir -p "$evidence_root" "$state_root"

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
cargo test --locked --manifest-path "$repo_root/Cargo.toml" -p manuvra-chrome \
  --test local_fixture -- --ignored

active_run=""
cleanup() {
  if [[ -n "$active_run" ]]; then
    (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_run") >/dev/null || true
  fi
}
trap cleanup EXIT

run_case() {
  local label=$1 fixture=$2 expected_exit=$3 expected_state=$4
  local provider_key=${5-${TYPESAFE_API_KEY-}}
  local request_component=$label
  if [[ $# -ge 5 ]]; then
    request_component=$5
  fi
  active_run="manuvra-browser-observation-$label-$stamp"
  local launch candidate output status
  launch=$(cd "$money_dir" && pnpm verify:app launch --run-id "$active_run" --port 4351)
  candidate=$(node -e 'const value=JSON.parse(process.argv[1]); process.stdout.write(value.result.candidate)' "$launch")
  (cd "$money_dir" && node scripts/app-driver.mjs doctor --run-id "$active_run" --candidate "$candidate") >"$evidence_root/$label-doctor.json"
  set +e
  TYPESAFE_API_KEY="$provider_key" XDG_STATE_HOME="$state_root/$label" "$repo_root/target/debug/manuvra" run \
    --request-id "browser-observation-$request_component-$stamp" --job "$repo_root/tests/live/$fixture" \
    --evidence "$evidence_root/$label" >"$evidence_root/$label-stdout.json" 2>"$evidence_root/$label-stderr.txt"
  status=$?
  set -e
  [[ $status -eq $expected_exit ]]
  output=$(node -e 'const fs=require("fs");const value=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));process.stdout.write(value.state)' "$evidence_root/$label-stdout.json")
  [[ "$output" == "$expected_state" ]]
  (cd "$money_dir" && node scripts/app-driver.mjs observe --run-id "$active_run" --feature accounts.empty) >"$evidence_root/$label-observe.json"
  (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_run") >"$evidence_root/$label-cleanup.json"
  active_run=""
}

run_case observe observe-money.json 0 passed
run_case dialog dialog-closed-money.json 4 failed
run_case secret secret-money.json 0 passed
run_case provider provider-redaction-money.json 0 passed live-provider-export-marker

if rg -a -F -- '+ Create account' \
  "$evidence_root/secret" "$evidence_root/secret-stdout.json" \
  "$evidence_root/secret-stderr.txt" "$state_root/secret"; then
  echo "classified marker leaked from the secret run" >&2
  exit 1
fi


if rg -a -F -- 'live-provider-export-marker' \
  "$evidence_root/provider" "$evidence_root/provider-stdout.json" \
  "$evidence_root/provider-stderr.txt" "$state_root/provider"; then
  echo "provider key leaked from the provider-redaction run" >&2
  exit 1
fi

secret_manifest=$(node -e 'const fs=require("fs");const value=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));process.stdout.write(value.evidence.manifest)' "$evidence_root/secret-stdout.json")
secret_dir=$(dirname "$secret_manifest")
diff -u <(jq -S . "$evidence_root/secret-stdout.json") <(jq -S . "$secret_dir/result.json")
jq -e '
  .target.kind == "browser" and
  .steps[0].done_when[0].scope == "viewport" and
  ([.values[].value] | all(. != "browser" and . != "viewport" and . != "passed" and . != "failed"))
' "$secret_dir/job.json" >/dev/null

echo "$evidence_root"

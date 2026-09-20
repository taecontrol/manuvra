#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-/home/guetteluis/Work/personal/money}
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/natural-done/$stamp"
state_root="$evidence_root/state"
report="$evidence_root/matrix.jsonl"
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

expected_target() {
  case "$1" in
    open) echo '+ Create account' ;;
    name) echo 'Account name' ;;
    currency) echo 'Currency or asset' ;;
    new-unit) echo '+ New currency or asset' ;;
    symbol) echo 'Symbol' ;;
    unit-name) echo 'Unit name' ;;
    use-unit) echo 'Use unit' ;;
    balance) echo 'Opening balance' ;;
    submit) echo 'Create account' ;;
    *) return 1 ;;
  esac
}

expected_operation() {
  case "$1" in
    name|symbol|unit-name|balance) echo TYPE_TEXT ;;
    *) echo CLICK ;;
  esac
}

assert_correct_stop() {
  local output=$1 payload run_dir step operation target_choice snapshot target_name expected
  payload=$(jq -r '.escalation.payload' "$output")
  [[ -f "$payload" ]]
  step=$(jq -r '.step_id' "$payload")
  operation=$(jq -r '.candidates.operation.choice' "$payload")
  [[ "$operation" == "$(expected_operation "$step")" ]]
  if [[ "$operation" == CLICK ]]; then
    target_choice=$(jq -r '.candidates.click_target.choice' "$payload")
  else
    target_choice=$(jq -r '.candidates.type_target.choice' "$payload")
  fi
  run_dir=$(dirname "$(dirname "$payload")")
  snapshot=$(jq -r '.observation.snapshot' "$payload")
  target_name=$(jq -r --argjson index "$target_choice" \
    '.elements[]|select(.index==$index)|.name' "$run_dir/$snapshot")
  expected=$(expected_target "$step")
  [[ "${target_name,,}" == *"${expected,,}"* ]]
}

assert_actions_correct() {
  local output=$1 run_dir trace step operation target expected
  run_dir=$(dirname "$(jq -r '.evidence.manifest' "$output")")
  trace="$run_dir/trace.jsonl"
  while IFS=$'\t' read -r step operation target; do
    [[ "$operation" == "$(expected_operation "$step")" ]]
    expected=$(expected_target "$step")
    [[ "${target,,}" == *"${expected,,}"* ]]
  done < <(jq -rs -r '
    reduce .[] as $event ({step:null, actions:[]};
      if ($event.event == "observation" or $event.event == "reobservation") then
        .step = $event.step_id
      elif $event.event == "action_prepared" then
        .actions += [[.step, $event.operation, $event.target.name]]
      else . end)
    | .actions[] | @tsv
  ' "$trace")
}

assert_complete_evidence() {
  local output=$1 manifest path expected actual
  manifest=$(jq -r '.evidence.manifest' "$output")
  jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" ]]
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [[ "$actual" == "$expected" ]]
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

run_case() {
  local iteration=$1 label="create-account-nl-$iteration"
  local launch candidate started finished status state classification output observe accounts units active_ms cleanup_status
  active_run="manuvra-natural-done-$iteration-$stamp"
  launch=$(cd "$money_dir" && pnpm verify:app launch --run-id "$active_run" --port 4351)
  candidate=$(jq -r '.result.candidate' <<<"$launch")
  (cd "$money_dir" && node scripts/app-driver.mjs doctor \
    --run-id "$active_run" --candidate "$candidate") >"$evidence_root/$label-doctor.json"
  started=$(date +%s%3N)
  set +e
  XDG_STATE_HOME="$state_root/$label" "$repo_root/target/debug/manuvra" run \
    --request-id "natural-done-$label-$stamp" \
    --job "$repo_root/tests/live/create-account-natural-done.json" \
    --evidence "$evidence_root/$label" \
    >"$evidence_root/$label-stdout.json" 2>"$evidence_root/$label-stderr.txt"
  status=$?
  set -e
  finished=$(date +%s%3N)
  [[ $status -eq 0 || $status -eq 2 ]]
  output="$evidence_root/$label-stdout.json"
  state=$(jq -r '.state' "$output")
  [[ "$state" == passed || "$state" == uncertain ]]
  classification=autonomous
  if [[ "$state" == uncertain ]]; then assert_correct_stop "$output"; fi
  if [[ "$state" == uncertain ]]; then classification=stopped; fi
  assert_actions_correct "$output"
  assert_complete_evidence "$output"
  (cd "$money_dir" && node scripts/app-driver.mjs observe \
    --run-id "$active_run" --feature accounts.create) >"$evidence_root/$label-observe.json"
  observe="$evidence_root/$label-observe.json"
  accounts=$(jq '.result.accounts|length' "$observe")
  units=$(jq '.result.units|length' "$observe")
  [[ $accounts -le 1 && $units -le 1 ]]
  if [[ "$state" == passed ]]; then [[ $accounts -eq 1 && $units -eq 1 ]]; fi
  active_ms=$(jq -s '[.[]|.active_ms // empty]|max // 0' \
    "$evidence_root/$label"/r_*/steps/*.json)
  (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_run") \
    >"$evidence_root/$label-cleanup.json"
  cleanup_status=$(jq -r '.status' "$evidence_root/$label-cleanup.json")
  active_run=""
  jq -cn --argjson iteration "$iteration" --arg state "$state" \
    --arg classification "$classification" \
    --argjson exit_code "$status" --argjson wall_ms "$((finished-started))" \
    --argjson active_ms "$active_ms" --argjson accounts "$accounts" \
    --argjson units "$units" --arg cleanup "$cleanup_status" \
    '{journey:"create-account-natural-language",run:$iteration,classification:$classification,state:$state,exit_code:$exit_code,wall_ms:$wall_ms,active_ms:$active_ms,accounts:$accounts,units:$units,cleanup:$cleanup}' \
    >>"$report"
}

for iteration in 1 2 3; do run_case "$iteration"; done

provider_key=${TYPESAFE_API_KEY-}
if [[ -n "$provider_key" ]] && rg -a -l -F -- "$provider_key" "$evidence_root" "$state_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

jq -s . "$report" >"$evidence_root/matrix.json"
echo "$evidence_root"

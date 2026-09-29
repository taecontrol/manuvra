#!/usr/bin/env bash
# Runs the create-account journey three times with natural-language done conditions and no final
# expectations. A Run passes autonomously or stops at the step it was working on without having
# prepared an action the job does not intend.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/live-lib.sh"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
preflight_fixture
require_command cargo
require_command shasum
require_provider_key
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/natural-done/$stamp"
state_root="$evidence_root/state"
report="$evidence_root/matrix.jsonl"
mkdir -p "$evidence_root" "$state_root"
trap cleanup_active_fixture EXIT

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
cargo test --locked --manifest-path "$repo_root/Cargo.toml" -p manuvra-chrome \
  --test local_fixture -- --ignored
manuvra="$repo_root/target/debug/manuvra"

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

# An offered candidate must be the step's intended operation. Without one, the escalation's
# `candidates` holds Jev's judgments, whose chosen operation and target must still fit the step.
assert_correct_stop() {
  local output=$1 payload run_dir step operation target_choice snapshot target_name expected
  payload=$(jq -r '.escalation.payload' "$output")
  [[ -f "$payload" ]]
  jq -e '.phase == "step"' "$payload" >/dev/null
  if jq -e '.offered_candidate != null' "$payload" >/dev/null; then
    candidate_is_expected "$payload"
    return
  fi
  jq -e '.candidates | type == "object"' "$payload" >/dev/null
  if ! jq -e '.candidates.operation.choice != null' "$payload" >/dev/null; then
    return 0
  fi
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
  target_name=$(jq -r --arg index "$target_choice" \
    '.elements[] | select("\(.index)" == $index) | .name' "$run_dir/$snapshot")
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
  done < <(jq -rs '
    reduce .[] as $event ({step:null, actions:[]};
      if ($event.event == "observation" or $event.event == "reobservation") then
        .step = $event.step_id
      elif $event.event == "action_prepared" then
        .actions += [[.step, $event.operation, $event.target.name]]
      else . end)
    | .actions[] | @tsv
  ' "$trace")
}

run_case() {
  local iteration=$1
  local label="create-account-nl-$iteration"
  local started finished status state classification output observe accounts units active_ms cleanup_status
  start_fixture "manuvra-natural-done-$iteration-$stamp" "$evidence_root/$label-fixture" \
    "$evidence_root/$label-launch.json" "$evidence_root/$label-doctor.json"
  started=$(date +%s%3N)
  set +e
  XDG_STATE_HOME="$state_root/$label" "$manuvra" run \
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
  if [[ "$state" == uncertain ]]; then
    assert_correct_stop "$output"
    classification=stopped
  fi
  assert_actions_correct "$output"
  verify_manifest "$output"
  observe="$evidence_root/$label-observe.json"
  observe_fixture accounts.create >"$observe"
  accounts=$(jq '.result.accounts|length' "$observe")
  units=$(jq '.result.units|length' "$observe")
  [[ $accounts -le 1 && $units -le 1 ]]
  if [[ "$state" == passed ]]; then [[ $accounts -eq 1 && $units -eq 1 ]]; fi
  active_ms=$(jq -s '[.[]|.active_ms // empty]|max // 0' \
    "$evidence_root/$label"/r_*/steps/*.json)
  stop_fixture "$evidence_root/$label-cleanup.json" "$evidence_root/$label-cleanup.stderr"
  cleanup_status=$(jq -r '.status' "$evidence_root/$label-cleanup.json")
  jq -cn --argjson iteration "$iteration" --arg state "$state" \
    --arg classification "$classification" \
    --argjson exit_code "$status" --argjson wall_ms "$((finished-started))" \
    --argjson active_ms "$active_ms" --argjson accounts "$accounts" \
    --argjson units "$units" --arg cleanup "$cleanup_status" \
    '{journey:"create-account-natural-language",run:$iteration,classification:$classification,state:$state,exit_code:$exit_code,wall_ms:$wall_ms,active_ms:$active_ms,accounts:$accounts,units:$units,cleanup:$cleanup}' \
    >>"$report"
}

for iteration in 1 2 3; do run_case "$iteration"; done

if provider_key_present "$evidence_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

jq -s . "$report" >"$evidence_root/matrix.json"
echo "$evidence_root"

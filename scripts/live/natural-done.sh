#!/usr/bin/env bash
# Runs the create-account journey three times with natural-language done conditions and no final
# expectations. A Run passes autonomously or stops at the step it was working on without having
# prepared an action the job does not intend.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

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
# `candidates` holds Jev's judgments. An abstention grants no mutation authority.
assert_correct_stop() {
  local output=$1 payload run_dir step operation target_choice snapshot target_name expected
  payload=$(jq -r '.escalation.payload' "$output")
  [[ -f "$payload" ]]
  # A final-verification stop has no step candidate to check.
  if jq -e '.phase == "verification"' "$payload" >/dev/null; then return 0; fi
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
  [[ "$operation" == "$(expected_operation "$step")" ]] || return 1
  if [[ "$operation" == CLICK ]]; then
    target_choice=$(jq -r '.candidates.click_target.choice' "$payload")
    if [[ "$target_choice" == NO_CLICK_TARGET ]]; then
      jq -e '.gate_reason == "done_uncertain" or .gate_reason == "click_target_unavailable"' "$payload" >/dev/null
      return
    fi
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
    [[ "$operation" == "$(expected_operation "$step")" ]] || return 1
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
  local job="$evidence_root/$label-job.json"
  fixture_job "$repo_root/tests/live/money/create-account-natural-done.json" "$job"
  set +e
  XDG_STATE_HOME="$state_root/$label" "$manuvra" run \
    --request-id "natural-done-$label-$stamp" \
    --job "$job" \
    --evidence "$evidence_root/$label" \
    >"$evidence_root/$label-stdout.json" 2>"$evidence_root/$label-stderr.txt"
  status=$?
  set -e
  [[ $status -eq 0 || $status -eq 2 || $status -eq 6 ]]
  output="$evidence_root/$label-stdout.json"
  status=$(settle_run "$state_root/$label" "$output")
  finished=$(date +%s%3N)
  [[ $status -eq 0 || $status -eq 2 ]]
  state=$(jq -r '.state' "$output")
  [[ "$state" == passed || "$state" == uncertain ]]
  classification=autonomous
  if [[ "$state" == uncertain ]]; then
    assert_correct_stop "$output"
    classification=stopped
    abort_paused_run "$state_root/$label" "$(jq -r '.run_id' "$output")" \
      "$evidence_root/$label-abort.json"
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

if [[ ${1:-} == --self-test ]]; then
  self_test_root=$(mktemp -d)
  for reason in done_uncertain click_target_unavailable provider_invalid_response; do
    jq -n --arg reason "$reason" '{phase:"step",step_id:"currency",gate_reason:$reason,
      offered_candidate:null,candidates:{operation:{choice:"CLICK"},click_target:{choice:"NO_CLICK_TARGET"}}}' >"$self_test_root/payload.json"
    jq -n --arg payload "$self_test_root/payload.json" '{escalation:{payload:$payload}}' >"$self_test_root/output.json"
    if assert_correct_stop "$self_test_root/output.json"; then
      [[ "$reason" != provider_invalid_response ]]
    else
      [[ "$reason" == provider_invalid_response ]]
    fi
  done
  for invalid in wrong_operation wrong_candidate; do
    jq -n --arg invalid "$invalid" '{phase:"step",step_id:"currency",gate_reason:"done_uncertain",
      offered_candidate:(if $invalid == "wrong_candidate" then {operation:"CLICK",target_name:"Wrong item"} else null end),
      candidates:{operation:{choice:(if $invalid == "wrong_operation" then "TYPE_TEXT" else "CLICK" end)},click_target:{choice:"NO_CLICK_TARGET"}}}' >"$self_test_root/payload.json"
    if assert_correct_stop "$self_test_root/output.json"; then exit 1; fi
  done
  rm -rf "$self_test_root"
  echo "natural-language abstention checks passed"
  exit 0
fi

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

for iteration in 1 2 3; do run_case "$iteration"; done

if provider_key_present "$evidence_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

jq -s . "$report" >"$evidence_root/matrix.json"
echo "$evidence_root"

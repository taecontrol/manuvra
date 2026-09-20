#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-/home/guetteluis/Work/personal/money}
runtime_root=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required for the headed Wayland run}
stamp=$(date +%Y%m%d-%H%M%S)-$$
live_root="$repo_root/.work/live/final-verification/$stamp"
mkdir -p "$live_root"

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
manuvra="$repo_root/target/debug/manuvra"
active_fixture=
cleanup() {
  if [[ -n "$active_fixture" ]]; then
    (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_fixture") >/dev/null || true
  fi
}
trap cleanup EXIT

wait_checkpoint() {
  local state_root=$1 run_id=$2 output=$3
  local state
  state=$(jq -r '.state' "$output")
  for attempt in $(seq 1 20); do
    [[ "$state" == running ]] || return 0
    local next="${output%.json}-progress-$attempt.json"
    set +e
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" status "$run_id" --wait-ms 30000 >"$next"
    local code=$?
    set -e
    [[ $code -eq 0 || $code -eq 2 || $code -eq 4 || $code -eq 5 || $code -eq 6 ]]
    cp "$next" "$output"
    state=$(jq -r '.state' "$output")
  done
  [[ "$state" != running ]]
}

candidate_is_expected() {
  local payload=$1
  jq -e '
    .offered_candidate as $c |
    (.step_id == "open" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Account name" and $c.value_name == "account_name") or
    (.step_id == "currency" and $c.operation == "CLICK" and $c.target_name == "Currency or asset") or
    (.step_id == "new-unit" and $c.operation == "CLICK" and $c.target_name == "+ New currency or asset") or
    (.step_id == "symbol" and $c.operation == "TYPE_TEXT" and $c.target_name == "Symbol" and $c.value_name == "unit_symbol") or
    (.step_id == "unit-name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Unit name" and $c.value_name == "unit_name") or
    (.step_id == "use-unit" and $c.operation == "CLICK" and $c.target_name == "Use unit") or
    (.step_id == "balance" and $c.operation == "TYPE_TEXT" and $c.target_name == "Opening balance" and $c.value_name == "opening_balance") or
    (.step_id == "submit" and $c.operation == "CLICK" and $c.target_name == "Create account")
  ' "$payload" >/dev/null
}

verify_manifest() {
  local result=$1
  local manifest
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '.complete == true and ([.artifacts[].complete] | all) and ([.artifacts[] | select(.role == "verification")] | length == 1)' "$manifest" >/dev/null
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" ]]
    [[ "$(sha256sum "$path" | cut -d' ' -f1)" == "$expected" ]]
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

run_case() {
  local label=$1 fixture=$2 expected_name=$3 secret=${4:-no}
  local case_root="$live_root/$label"
  local state_root="$case_root/state"
  local evidence_root="$case_root/evidence"
  mkdir -p "$case_root" "$state_root" "$evidence_root"

  active_fixture="manuvra-final-verification-$label-$stamp"
  local launch candidate
  launch=$(cd "$money_dir" && pnpm verify:app launch --run-id "$active_fixture" --port 4351)
  candidate=$(jq -r '.result.candidate' <<<"$launch")
  (cd "$money_dir" && node scripts/app-driver.mjs doctor \
    --run-id "$active_fixture" --candidate "$candidate") >"$case_root/doctor.json"

  local request="final-verification-$label-$stamp"
  local result="$case_root/current-result.json"
  local started_ms ended_ms
  started_ms=$(date +%s%3N)
  set +e
  XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
    "$manuvra" run --request-id "$request" --job "$fixture" --evidence "$evidence_root" \
    >"$result"
  local code=$?
  set -e
  [[ $code -eq 0 || $code -eq 2 || $code -eq 6 ]]
  cp "$result" "$case_root/initial-result.json"
  local run_id
  run_id=$(jq -r '.run_id' "$result")
  wait_checkpoint "$state_root" "$run_id" "$result"

  local assists=0
  while [[ $(jq -r '.state' "$result") != passed ]]; do
    jq -e '.state == "uncertain" and .terminal == false' "$result" >/dev/null
    assists=$((assists + 1))
    [[ $assists -le 16 ]]
    local escalation_id phase payload disposition request_id resume_result resume_code
    escalation_id=$(jq -r '.escalation.id' "$result")
    phase=$(jq -r '.escalation.phase' "$result")
    payload=$(jq -r '.escalation.payload' "$result")
    disposition="$case_root/disposition-$assists.json"
    if [[ "$phase" == verification ]]; then
      jq -e '(.escalation.dispositions | index("execute")) == null and (.escalation.dispositions | index("advance")) != null' "$result" >/dev/null
      jq -n --arg escalation "$escalation_id" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"advance",rationale:"Caller directly attests the final visible account facts in this isolated fixture."}}' \
        >"$disposition"
    elif jq -e '.offered_candidate != null' "$payload" >/dev/null; then
      candidate_is_expected "$payload"
      jq -n --arg escalation "$escalation_id" \
        --arg candidate "$(jq -r '.offered_candidate.id' "$payload")" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"execute",candidate_id:$candidate}}' \
        >"$disposition"
    else
      jq -n --arg escalation "$escalation_id" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"retry_observation"}}' \
        >"$disposition"
    fi
    request_id="final-verification-$label-resume-$assists-$stamp"
    resume_result="$case_root/resume-$assists.json"
    set +e
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" resume "$run_id" --request-id "$request_id" --input "$disposition" \
      >"$resume_result"
    resume_code=$?
    set -e
    [[ $resume_code -eq 0 || $resume_code -eq 2 || $resume_code -eq 6 ]]
    cp "$resume_result" "$result"
    wait_checkpoint "$state_root" "$run_id" "$result"
  done
  ended_ms=$(date +%s%3N)

  jq -e --arg run_id "$run_id" '
    .run_id == $run_id and .state == "passed" and .terminal == true and
    .evidence.complete == true and .verdict.overall == "satisfied" and
    ([.verdict.steps[].result] | all(. == "satisfied")) and
    ([.verdict.expectations[].result] | all(. == "satisfied"))
  ' "$result" >/dev/null
  if (( assists > 0 )); then
    jq -e '.verdict.caller_assisted == true' "$result" >/dev/null
  fi
  verify_manifest "$result"

  local manifest verification active_ms classification
  manifest=$(jq -r '.evidence.manifest' "$result")
  verification=$(jq -r '.artifacts[] | select(.role == "verification") | .path' "$manifest")
  jq -e '.phase == "verification" and ([.expectations[].result] | all(. == "satisfied"))' "$verification" >/dev/null

  (cd "$money_dir" && node scripts/app-driver.mjs observe \
    --run-id "$active_fixture" --feature accounts.create) >"$case_root/observe.json"
  jq -e --arg name "$expected_name" \
    '([.result.accounts[] | select(.name == $name)] | length) == 1 and (.result.accounts | length) == 1 and (.result.units | length) == 1' \
    "$case_root/observe.json" >/dev/null

  if [[ "$secret" == yes ]]; then
    local secret_numeric_marker=${expected_name##* }
    ! rg -a -F -- "$expected_name" "$(dirname "$manifest")"
    ! rg -a -F -- "$expected_name" "$verification"
    jq -e '.provider.request | tostring | contains("<value:account_name>")' "$verification" >/dev/null
    ! jq -e --arg marker "$secret_numeric_marker" \
      '.. | objects | .literal? // empty | select(. == $marker)' "$verification" "$result" >/dev/null
    rg -a -q '<masked:[0-9]+>|<value:account_name>' "$(dirname "$manifest")"
  fi

  active_ms=$(jq -s '[.[] | .active_ms // 0] | max // 0' $(jq -r '.artifacts[] | select(.role == "step") | .path' "$manifest"))
  classification=autonomous
  (( assists == 0 )) || classification=assisted
  jq -n --arg label "$label" --arg run_id "$run_id" --arg classification "$classification" \
    --argjson assists "$assists" --argjson active_ms "$active_ms" \
    --argjson wall_ms "$((ended_ms-started_ms))" \
    '{label:$label,run_id:$run_id,classification:$classification,assists:$assists,active_ms:$active_ms,wall_ms:$wall_ms,state:"passed"}' \
    >>"$live_root/matrix.jsonl"

  (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_fixture") >"$case_root/cleanup.json"
  rmdir "$runtime_root/manuvra/runs/$run_id"
  active_fixture=
}

for index in 1 2 3; do
  run_case "create-unit-$index" "$repo_root/tests/live/create-unit.json" "Unit seed wallet"
done
for index in 1 2 3; do
  run_case "create-account-$index" "$repo_root/tests/live/create-account.json" "Review wallet"
done
run_case "create-account-secret" "$repo_root/tests/live/create-account.secret.json" "Secret review wallet 7491" yes

jq -s '{schema_version:1,journey:"final_verification",runs:.,counts:(group_by(.classification)|map({key:.[0].classification,value:length})|from_entries)}' \
  "$live_root/matrix.jsonl" >"$live_root/matrix.json"

if [[ -n ${TYPESAFE_API_KEY-} ]] && rg -a -l -F -- "$TYPESAFE_API_KEY" "$live_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi
if rg -a -l '"(document_id|node_id|target_node_id)"' "$live_root"; then
  echo "internal browser identity leaked into live output or evidence" >&2
  exit 1
fi

echo "$live_root"

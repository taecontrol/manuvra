#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-/home/guetteluis/Work/personal/money}
runtime_root=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required for the headed Wayland run}
stamp=$(date +%Y%m%d-%H%M%S)-$$
live_root="$repo_root/.work/live/resume-dispositions/$stamp"
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
    [[ $code -eq 0 || $code -eq 2 || $code -eq 5 || $code -eq 6 ]]
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
    (.step_id == "submit" and $c.operation == "CLICK" and $c.target_name == "Create account")
  ' "$payload" >/dev/null
}

verify_manifest() {
  local result=$1
  local manifest
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" ]]
    [[ "$(sha256sum "$path" | cut -d' ' -f1)" == "$expected" ]]
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

run_case() {
  local label=$1 fixture=$2 forced=$3
  local case_root="$live_root/$label"
  local state_root="$case_root/state"
  local evidence_root="$case_root/evidence"
  local job="$fixture"
  mkdir -p "$case_root" "$state_root" "$evidence_root"

  active_fixture="manuvra-resume-dispositions-$label-$stamp"
  local launch candidate
  launch=$(cd "$money_dir" && pnpm verify:app launch --run-id "$active_fixture" --port 4351)
  candidate=$(jq -r '.result.candidate' <<<"$launch")
  (cd "$money_dir" && node scripts/app-driver.mjs doctor \
    --run-id "$active_fixture" --candidate "$candidate") >"$case_root/doctor.json"

  if [[ "$forced" == yes ]]; then
    job="$case_root/job.json"
    jq '.options.pause_timeout_ms=120000 | .options.lifetime_ms=300000' "$fixture" >"$job"
  fi

  local request="resume-dispositions-$label-$stamp"
  local result="$case_root/result.json"
  set +e
  XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
    "$manuvra" run --request-id "$request" --job "$job" --evidence "$evidence_root" \
    >"$result"
  local code=$?
  set -e
  [[ $code -eq 0 || $code -eq 2 || $code -eq 6 ]]
  local run_id
  run_id=$(jq -r '.run_id' "$result")
  wait_checkpoint "$state_root" "$run_id" "$result"

  if [[ "$forced" == yes ]]; then
    jq -e '.state == "uncertain" and .terminal == false and .reason.code == "debug_forced_stop" and .escalation.step_id == "submit" and (.escalation.dispositions | index("execute")) != null' "$result" >/dev/null
  fi

  local assists=0 first_resume= first_resume_request=
  while [[ $(jq -r '.state' "$result") != passed ]]; do
    jq -e '.state == "uncertain" and .terminal == false' "$result" >/dev/null
    assists=$((assists + 1))
    [[ $assists -le 12 ]]
    local escalation_id payload disposition request_id resume_result resume_code
    escalation_id=$(jq -r '.escalation.id' "$result")
    payload=$(jq -r '.escalation.payload' "$result")
    disposition="$case_root/disposition-$assists.json"
    if jq -e '.offered_candidate != null' "$payload" >/dev/null; then
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
    request_id="resume-dispositions-$label-resume-$assists-$stamp"
    resume_result="$case_root/resume-$assists.json"
    if [[ "$forced" == yes && $assists -eq 1 ]]; then
      local contender_request="resume-dispositions-$label-contender-$stamp"
      local contender_result="$case_root/resume-contender.json"
      set +e
      (XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
        "$manuvra" resume "$run_id" --request-id "$request_id" --input "$disposition" \
        >"$resume_result"; echo $? >"$resume_result.code") &
      local primary_pid=$!
      (XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
        "$manuvra" resume "$run_id" --request-id "$contender_request" --input "$disposition" \
        >"$contender_result"; echo $? >"$contender_result.code") &
      local contender_pid=$!
      wait "$primary_pid"
      wait "$contender_pid"
      set -e
      local primary_code contender_code
      primary_code=$(cat "$resume_result.code")
      contender_code=$(cat "$contender_result.code")
      if [[ $primary_code -eq 64 ]]; then
        [[ $contender_code -eq 0 || $contender_code -eq 2 || $contender_code -eq 6 ]]
        jq -e '.error.code == "stale_escalation"' "$resume_result" >/dev/null
        resume_result=$contender_result
        resume_code=$contender_code
        request_id=$contender_request
      else
        [[ $primary_code -eq 0 || $primary_code -eq 2 || $primary_code -eq 6 ]]
        [[ $contender_code -eq 64 ]]
        jq -e '.error.code == "stale_escalation"' "$contender_result" >/dev/null
        resume_code=$primary_code
      fi
    else
      set +e
      XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
        "$manuvra" resume "$run_id" --request-id "$request_id" --input "$disposition" \
        >"$resume_result"
      resume_code=$?
      set -e
    fi
    [[ $resume_code -eq 0 || $resume_code -eq 2 || $resume_code -eq 6 ]]
    if [[ -z "$first_resume" ]]; then
      first_resume=$resume_result
      first_resume_request=$request_id
    fi
    cp "$resume_result" "$result"
    wait_checkpoint "$state_root" "$run_id" "$result"
  done

  jq -e --arg run_id "$run_id" '.run_id == $run_id and .state == "passed" and .terminal == true' "$result" >/dev/null
  if (( assists > 0 )); then
    jq -e '.verdict.caller_assisted == true' "$result" >/dev/null
  fi
  verify_manifest "$result"

  (cd "$money_dir" && node scripts/app-driver.mjs observe \
    --run-id "$active_fixture" --feature accounts.create) >"$case_root/observe.json"
  jq -e --arg name "$(if [[ "$forced" == yes ]]; then echo 'Review wallet'; else echo 'Unit seed wallet'; fi)" \
    '([.result.accounts[] | select(.name == $name)] | length) == 1 and (.result.accounts | length) == 1 and (.result.units | length) == 1' \
    "$case_root/observe.json" >/dev/null

  if [[ "$forced" == yes ]]; then
    [[ $assists -ge 1 ]]
    local manifest trace disposition_count
    manifest=$(jq -r '.evidence.manifest' "$result")
    trace=$(jq -r '.artifacts[] | select(.role == "trace") | .path' "$manifest")
    jq -s -e '[.[] | select(.event == "action_prepared" and .basis == "caller_authority" and .target.name == "Create account")] | length == 1' "$trace" >/dev/null
    disposition_count=$(jq '[.artifacts[] | select(.role == "disposition")] | length' "$manifest")
    [[ $disposition_count -ge 1 ]]

    local first_disposition="$case_root/disposition-1.json"
    set +e
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" resume "$run_id" --request-id "$first_resume_request" \
      --input "$first_disposition" >"$case_root/resume-dedup.json"
    local dedup_code=$?
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" resume "$run_id" --request-id "resume-dispositions-$label-stale-$stamp" \
      --input "$first_disposition" >"$case_root/resume-stale.json"
    local stale_code=$?
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" resume "$run_id" --request-id "resume-dispositions-$label-stale-$stamp" \
      --input "$first_disposition" >"$case_root/resume-stale-dedup.json"
    local stale_dedup_code=$?
    jq -n --arg escalation "$escalation_id" \
      '{schema_version:1,escalation_id:$escalation,disposition:{kind:"abort"}}' \
      >"$case_root/disposition-stale-conflict.json"
    XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
      "$manuvra" resume "$run_id" --request-id "resume-dispositions-$label-stale-$stamp" \
      --input "$case_root/disposition-stale-conflict.json" \
      >"$case_root/resume-stale-conflict.json"
    local stale_conflict_code=$?
    set -e
    [[ $dedup_code -eq $resume_code ]]
    cmp -s "$first_resume" "$case_root/resume-dedup.json"
    jq -e '.verdict.caller_assisted == true' "$case_root/resume-dedup.json" >/dev/null
    [[ $stale_code -eq 64 ]]
    jq -e '.error.code == "stale_escalation"' "$case_root/resume-stale.json" >/dev/null
    [[ $stale_dedup_code -eq 64 ]]
    cmp -s "$case_root/resume-stale.json" "$case_root/resume-stale-dedup.json"
    [[ $stale_conflict_code -eq 64 ]]
    jq -e '.error.code == "request_conflict"' "$case_root/resume-stale-conflict.json" >/dev/null
  fi

  printf '%s\t%s\t%s\t%s\n' "$label" "$run_id" "$assists" "$(jq -r '.state' "$result")" >>"$live_root/matrix.tsv"
  (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_fixture") >"$case_root/cleanup.json"
  rmdir "$runtime_root/manuvra/runs/$run_id"
  active_fixture=
}

run_case forced-account "$repo_root/tests/live/create-account-forced-pause.json" yes
for index in 1 2 3; do
  run_case "create-unit-$index" "$repo_root/tests/live/create-unit.expectation-free.json" no
done

if [[ -n ${TYPESAFE_API_KEY-} ]] && rg -a -l -F -- "$TYPESAFE_API_KEY" "$live_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

if rg -a -l '"(document_id|node_id|target_node_id)"' "$live_root"; then
  echo "internal browser identity leaked into live output or evidence" >&2
  exit 1
fi

echo "$live_root"

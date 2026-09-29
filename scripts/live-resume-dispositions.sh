#!/usr/bin/env bash
# Drives the forced create-account escalation through `resume`: two concurrent resumes race for one
# escalation, then the first resume is replayed, a stale escalation is resumed twice, and the stale
# request id is reused with a different disposition.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/live-lib.sh"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
preflight_fixture
require_command cargo
require_command rg
require_command shasum
require_provider_key
runtime_root=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required for background runs}
stamp=$(date +%Y%m%d-%H%M%S)-$$
live_root="$repo_root/.work/live/resume-dispositions/$stamp"
mkdir -p "$live_root"
trap cleanup_active_fixture EXIT

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
manuvra="$repo_root/target/debug/manuvra"

label=forced-account
case_root="$live_root/$label"
state_root="$case_root/state"
evidence_root="$case_root/evidence"
job="$case_root/job.json"
mkdir -p "$case_root" "$state_root" "$evidence_root"

start_fixture "manuvra-resume-dispositions-$label-$stamp" "$case_root/money-fixture" \
  "$case_root/launch.json" "$case_root/doctor.json"
jq '.options.pause_timeout_ms=120000 | .options.lifetime_ms=300000' \
  "$repo_root/tests/live/create-account-forced-pause.json" >"$job"

result="$case_root/result.json"
set +e
XDG_STATE_HOME="$state_root" \
  "$manuvra" run --request-id "resume-dispositions-$label-$stamp" --job "$job" \
  --evidence "$evidence_root" >"$result"
code=$?
set -e
[[ $code -eq 0 || $code -eq 2 || $code -eq 6 ]]
run_id=$(jq -r '.run_id' "$result")
wait_checkpoint "$state_root" "$run_id" "$result"
jq -e '.state == "uncertain" and .terminal == false and .reason.code == "debug_forced_stop" and .escalation.step_id == "submit" and (.escalation.dispositions | index("execute")) != null' "$result" >/dev/null

# Two concurrent resumes of the first escalation: exactly one wins, the other sees it as stale.
resume_contended() {
  local disposition=$1 contender_request="resume-dispositions-$label-contender-$stamp"
  local contender_result="$case_root/resume-contender.json" primary_pid contender_pid
  local primary_code contender_code
  set +e
  (XDG_STATE_HOME="$state_root" \
    "$manuvra" resume "$run_id" --request-id "$request_id" --input "$disposition" \
    >"$resume_result"; echo $? >"$resume_result.code") &
  primary_pid=$!
  (XDG_STATE_HOME="$state_root" \
    "$manuvra" resume "$run_id" --request-id "$contender_request" --input "$disposition" \
    >"$contender_result"; echo $? >"$contender_result.code") &
  contender_pid=$!
  wait "$primary_pid"
  wait "$contender_pid"
  set -e
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
}

assists=0
first_resume=
first_resume_request=
first_resume_code=
while [[ $(jq -r '.state' "$result") != passed ]]; do
  jq -e '.state == "uncertain" and .terminal == false' "$result" >/dev/null
  assists=$((assists + 1))
  [[ $assists -le 12 ]]
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
  if [[ $assists -eq 1 ]]; then
    resume_contended "$disposition"
  else
    set +e
    XDG_STATE_HOME="$state_root" \
      "$manuvra" resume "$run_id" --request-id "$request_id" --input "$disposition" \
      >"$resume_result"
    resume_code=$?
    set -e
  fi
  [[ $resume_code -eq 0 || $resume_code -eq 2 || $resume_code -eq 6 ]]
  if [[ -z "$first_resume" ]]; then
    first_resume=$resume_result
    first_resume_request=$request_id
    first_resume_code=$resume_code
  fi
  cp "$resume_result" "$result"
  wait_checkpoint "$state_root" "$run_id" "$result"
done

jq -e --arg run_id "$run_id" '.run_id == $run_id and .state == "passed" and .terminal == true' "$result" >/dev/null
jq -e '.verdict.caller_assisted == true' "$result" >/dev/null
verify_manifest "$result"

observe_fixture accounts.create >"$case_root/observe.json"
jq -e '([.result.accounts[] | select(.name == "Review wallet")] | length) == 1 and (.result.accounts | length) == 1 and (.result.units | length) == 1' \
  "$case_root/observe.json" >/dev/null

manifest=$(jq -r '.evidence.manifest' "$result")
trace=$(jq -r '.artifacts[] | select(.role == "trace") | .path' "$manifest")
jq -s -e '[.[] | select(.event == "action_prepared" and .basis == "caller_authority" and .target.name == "Create account")] | length == 1' "$trace" >/dev/null
disposition_count=$(jq '[.artifacts[] | select(.role == "disposition")] | length' "$manifest")
[[ $disposition_count -ge 1 ]]

# Replaying the first resume returns its recorded response; a new request for the consumed
# escalation is stale, replays as stale, and conflicts when its id is reused with other input.
first_disposition="$case_root/disposition-1.json"
stale_request="resume-dispositions-$label-stale-$stamp"
set +e
XDG_STATE_HOME="$state_root" \
  "$manuvra" resume "$run_id" --request-id "$first_resume_request" \
  --input "$first_disposition" >"$case_root/resume-dedup.json"
dedup_code=$?
XDG_STATE_HOME="$state_root" \
  "$manuvra" resume "$run_id" --request-id "$stale_request" \
  --input "$first_disposition" >"$case_root/resume-stale.json"
stale_code=$?
XDG_STATE_HOME="$state_root" \
  "$manuvra" resume "$run_id" --request-id "$stale_request" \
  --input "$first_disposition" >"$case_root/resume-stale-dedup.json"
stale_dedup_code=$?
jq -n --arg escalation "$escalation_id" \
  '{schema_version:1,escalation_id:$escalation,disposition:{kind:"abort"}}' \
  >"$case_root/disposition-stale-conflict.json"
XDG_STATE_HOME="$state_root" \
  "$manuvra" resume "$run_id" --request-id "$stale_request" \
  --input "$case_root/disposition-stale-conflict.json" \
  >"$case_root/resume-stale-conflict.json"
stale_conflict_code=$?
set -e
[[ $dedup_code -eq $first_resume_code ]]
cmp -s "$first_resume" "$case_root/resume-dedup.json"
jq -e '.verdict.caller_assisted == true' "$case_root/resume-dedup.json" >/dev/null
[[ $stale_code -eq 64 ]]
jq -e '.error.code == "stale_escalation"' "$case_root/resume-stale.json" >/dev/null
[[ $stale_dedup_code -eq 64 ]]
cmp -s "$case_root/resume-stale.json" "$case_root/resume-stale-dedup.json"
[[ $stale_conflict_code -eq 64 ]]
jq -e '.error.code == "request_conflict"' "$case_root/resume-stale-conflict.json" >/dev/null

printf '%s\t%s\t%s\t%s\n' "$label" "$run_id" "$assists" "$(jq -r '.state' "$result")" >>"$live_root/matrix.tsv"
stop_fixture "$case_root/cleanup.json" "$case_root/cleanup.stderr"
rmdir "$runtime_root/manuvra/runs/$run_id"

if provider_key_present "$live_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi

if rg -a -l '"(document_id|node_id|target_node_id)"' "$live_root"; then
  echo "internal browser identity leaked into live output or evidence" >&2
  exit 1
fi

echo "$live_root"

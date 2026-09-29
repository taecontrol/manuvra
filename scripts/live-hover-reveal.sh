#!/usr/bin/env bash
# Release gate for hover-revealed controls: the synthetic row-action journey runs three times with
# Jev against tests/fixtures/browser-hover-reveal.html. Every run counts. A failed or stopped run is
# recorded with its evidence and is never retried or discarded; the script exits nonzero if any run
# fails a check.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/live-lib.sh"
require_provider_key

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
runs=3
stamp=$(date +%Y%m%d-%H%M%S)-$$
live_root="$repo_root/.work/live/hover-reveal/$stamp"
mkdir -p "$live_root"
exec > >(tee -a "$live_root/invocation.log") 2>&1
# Control sockets and Chromium profiles need short paths, so each run's runtime directory lives in
# a private scratch root. Its path also identifies every process this script started.
scratch=$(mktemp -d /tmp/manuvra-hover.XXXXXX)
manuvra="$repo_root/target/release/manuvra"
server_pid=
active_run=
active_state=
active_runtime=

manuvra_in_run() {
  XDG_STATE_HOME="$active_state" XDG_RUNTIME_DIR="$active_runtime" TMPDIR="$active_runtime/tmp" \
    "$manuvra" "$@"
}

# Prints the pids whose runtime directory is $1 or lies beneath it, plus any Chromium whose profile
# lies beneath it. Only processes started by this script carry these paths, so a concurrent Manuvra
# session elsewhere is never matched.
processes_under() {
  local root=$1
  {
    grep -l -s -z -x -F -- "XDG_RUNTIME_DIR=$root" /proc/[0-9]*/environ || true
    grep -l -s -z -F -- "XDG_RUNTIME_DIR=$root/" /proc/[0-9]*/environ || true
  } | sed -n 's#^/proc/\([0-9]*\)/environ$#\1#p'
  pgrep -f -- "--user-data-dir=$root/" || true
}

cleanup() {
  set +e
  if [[ -n "$active_run" ]]; then
    manuvra_in_run abort "$active_run" --request-id "hover-reveal-cleanup-$stamp" >/dev/null 2>&1
  fi
  local pids
  pids=$(processes_under "$scratch" | sort -u)
  if [[ -n "$pids" ]]; then
    printf '%s\n' "$pids" >"$live_root/cleanup-killed-pids.txt"
    xargs -r kill -KILL <<<"$pids" 2>/dev/null
  fi
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null
    wait "$server_pid" 2>/dev/null
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

cargo build --release --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
revision=$(git -C "$repo_root" rev-parse HEAD)
if [[ -n "$(git -C "$repo_root" status --porcelain --untracked-files=no)" ]]; then
  revision="$revision+dirty"
fi
jq -n --arg revision "$revision" \
  --arg binary "$manuvra" \
  --arg binary_sha256 "$(sha256sum "$manuvra" | cut -d' ' -f1)" \
  --arg version "$("$manuvra" version)" \
  --arg browser "$(chromium --version)" \
  '{revision:$revision,binary:$binary,binary_sha256:$binary_sha256,version:$version,browser:$browser,headless:true}' \
  >"$live_root/environment.json"

python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$repo_root/tests/fixtures" \
  >"$live_root/fixture-server.log" 2>&1 &
server_pid=$!
port=
for _ in $(seq 1 100); do
  port=$(sed -n 's/^Serving HTTP on 127\.0\.0\.1 port \([0-9][0-9]*\) .*/\1/p' "$live_root/fixture-server.log")
  [[ -z "$port" ]] || break
  kill -0 "$server_pid"
  sleep 0.05
done
[[ -n "$port" ]]
origin="http://127.0.0.1:$port"
curl -fsS -o /dev/null "$origin/browser-hover-reveal.html"

job="$live_root/job.json"
jq --arg origin "$origin" --arg revision "$revision" '
  .target.url = ($origin + "/browser-hover-reveal.html") |
  .context.revision = $revision |
  .options.allowed_origins = [$origin]
' "$repo_root/tests/live/hover-reveal.template.json" >"$job"
if grep -q -F '{{' "$job"; then
  echo "job template placeholder left unresolved" >&2
  exit 1
fi

wait_terminal() {
  local run_id=$1 result=$2 label=$3
  local state attempt code next
  state=$(jq -r '.state' "$result")
  for attempt in $(seq 1 40); do
    [[ "$state" == running ]] || return 0
    next="${result%.json}-progress-$attempt.json"
    set +e
    manuvra_in_run status "$run_id" --wait-ms 30000 >"$next"
    code=$?
    set -e
    printf 'status %s exit %s\n' "$attempt" "$code" >>"$live_root/$label/exit-codes.txt"
    [[ $code -eq 0 || $code -eq 2 || $code -eq 3 || $code -eq 4 || $code -eq 5 || $code -eq 6 ]]
    cp "$next" "$result"
    state=$(jq -r '.state' "$result")
  done
  [[ "$state" != running ]]
}

manifest_is_complete() {
  local result=$1 manifest path expected
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '.evidence.complete == true' "$result" >/dev/null &&
    jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null || return 1
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" && "$(sha256sum "$path" | cut -d' ' -f1)" == "$expected" ]] || return 1
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

# The verdict judges exactly the job's steps and expectations, in order, and satisfies each one.
every_step_and_expectation_satisfied() {
  jq -e --slurpfile job "$2" '
    [.verdict.steps[] | [.id, .result]] == [$job[0].steps[] | [.id, "satisfied"]] and
    [.verdict.expectations[] | [.id, .result]] == [$job[0].expectations[] | [.id, "satisfied"]]
  ' "$1" >/dev/null
}

# A passed run publishes every evidence role, one step record per job step, and no escalation.
manifest_covers_journey() {
  jq -e '
    ([.artifacts[].role] as $roles |
      ["normalized_job","provenance","observation","screenshot","decision","trace","verification","cleanup","result"] |
      all(. as $role | $roles | index($role) != null)) and
    ([.artifacts[] | select(.role == "step")] | length == 4) and
    ([.artifacts[] | select(.role == "escalation")] | length == 0)
  ' "$(jq -r '.evidence.manifest' "$1")" >/dev/null
}

# The journey 1 and journey 3 order: each row is hovered before its revealed control is clicked.
hover_sequence_holds() {
  jq -s -e '
    [.[] | select(.event == "action_prepared")][0:4] |
    map(if .operation == "HOVER" then "HOVER \(.hover_target.name)" else "\(.operation) \(.target.name)" end) ==
      ["HOVER Groceries", "CLICK Actions for Groceries", "HOVER Rent", "CLICK Actions for Rent"]
  ' "$1" >/dev/null
}

# Pairs the observation that preceded the Rent hover with its artifact. Every capture publishes one
# trace event carrying `done` (or the final verification event) and one observation artifact, in the
# same order, so the counts must agree.
observation_before_rent_hover() {
  local trace=$1 manifest=$2 index
  index=$(jq -s '
    reduce .[] as $e ({seen: 0, hit: null};
      if .hit != null then .
      elif ($e | has("done")) or $e.event == "final_verification_observation" then .seen += 1
      elif ($e.event == "action_prepared" and $e.operation == "HOVER" and $e.hover_target.name == "Rent") then .hit = .seen
      else . end) | .hit // empty
  ' "$trace")
  [[ -n "$index" && $index -gt 0 ]] || return 1
  jq -e -n --slurpfile trace "$trace" --slurpfile manifest "$manifest" '
    ([$trace[] | select(has("done") or .event == "final_verification_observation")] | length) ==
    ([$manifest[0].artifacts[] | select(.role == "observation")] | length)
  ' >/dev/null || return 1
  jq -r --argjson index "$index" \
    '[.artifacts[] | select(.role == "observation")][$index - 1].path' "$manifest"
}

rent_hover_started_from_groceries_revealed() {
  [[ -f "$1" ]] && jq -e '
    ([.elements[].name] | index("Actions for Groceries") != null and index("Actions for Rent") == null) and
    any(.hover_regions[]?; .name == "Rent" and (.reveals_on_hover | index("Actions for Rent") != null))
  ' "$1" >/dev/null
}

no_groceries_click_from_rent_step() {
  jq -s -e '
    any(.[]; .event == "observation" and .step_id == "rent-menu") and
    ([foreach .[] as $e (false;
        . or ($e.event == "observation" and $e.step_id == "rent-menu");
        if . and $e.event == "action_prepared" and $e.operation == "CLICK" and
           (($e.target.name // "") | contains("Groceries")) then $e else empty end)] | length == 0)
  ' "$1" >/dev/null
}

# Succeeds only when a search of every file, hidden and ignored ones included, completed without
# a match. A search that could not complete is a failure, never a clean result.
absent_from() {
  local status=0
  rg -a -uuu -q "$@" || status=$?
  [[ $status -eq 1 ]]
}

no_internal_identity_in() {
  absent_from -- '"(document_id|node_id|target_node_id)"' "$1"
}

no_provider_key_in() {
  ! provider_key_present "$1"
}

# Waits for the run's host, watchdog, and Chromium to exit, then requires that Manuvra's Chromium
# profile and runtime entries are gone. Chromium's own org.chromium.* scratch entries are recorded
# but not judged: Chromium leaves them behind on termination regardless of this feature.
no_leftovers() {
  local runtime=$1 run_id=$2 report=$3 deadline remaining
  deadline=$(($(date +%s) + 20))
  while :; do
    remaining=$(processes_under "$runtime" | sort -u)
    [[ -n "$remaining" && $(date +%s) -lt $deadline ]] || break
    sleep 0.2
  done
  {
    printf 'processes:%s\n' "$(tr '\n' ' ' <<<"$remaining")"
    printf 'profiles:%s\n' "$(find "$runtime/tmp" -mindepth 1 -maxdepth 1 -printf ' %f')"
    printf 'runtime:%s\n' "$(find "$runtime/manuvra/runs/$run_id" -mindepth 1 -printf ' %f' 2>/dev/null)"
  } >"$report"
  [[ -z "$remaining" ]] &&
    [[ -z "$(find "$runtime/tmp" -mindepth 1 -maxdepth 1 -name 'manuvra-chromium-*')" ]] &&
    [[ -z "$(find "$runtime/manuvra/runs/$run_id" -mindepth 1 2>/dev/null)" ]]
}

run_ok=
checks=
check() {
  local name=$1
  shift
  if "$@" >/dev/null; then
    printf '%s\tpass\n' "$name" >>"$checks"
  else
    printf '%s\tfail\n' "$name" >>"$checks"
    run_ok=false
  fi
}

run_case() {
  local index=$1
  local label="run-$index"
  local case_root="$live_root/$label"
  active_state="$case_root/state"
  active_runtime="$scratch/$label"
  mkdir -p "$active_state" "$case_root/evidence" "$active_runtime/tmp"
  chmod 700 "$active_runtime"
  checks="$case_root/checks.tsv"
  run_ok=true
  : >"$checks"

  local result="$case_root/result.json" code started ended run_id
  started=$(date +%s%3N)
  set +e
  manuvra_in_run run --request-id "hover-reveal-$label-$stamp" --job "$job" \
    --evidence "$case_root/evidence" --headless >"$result"
  code=$?
  set -e
  printf 'run exit %s\n' "$code" >"$case_root/exit-codes.txt"
  if [[ $code -eq 64 || $code -eq 70 ]] || ! run_id=$(jq -e -r '.run_id' "$result"); then
    printf 'harness\tfail\n' >>"$checks"
    jq -n --arg label "$label" --argjson code "$code" \
      '{label:$label,outcome:"harness_failure",run_exit:$code,ok:false}' >>"$live_root/matrix.jsonl"
    active_run=
    return 0
  fi
  active_run=$run_id
  jq -c '{host_pid:.host.pid,watchdog_pid:.watchdog.pid}' \
    "$active_state/manuvra/runs/$run_id/control.json" >"$case_root/control-pids.json" 2>/dev/null || true
  wait_terminal "$run_id" "$result" "$label"

  # Caller assistance is outside this gate: an escalation is recorded as a stop and the run aborted.
  local outcome
  outcome=$(jq -r 'if .terminal then .state else "stopped_\(.state)" end' "$result")
  if [[ "$(jq -r '.terminal' "$result")" != true ]]; then
    cp "$result" "$case_root/stop.json"
    set +e
    manuvra_in_run abort "$run_id" --request-id "hover-reveal-$label-abort-$stamp" >"$case_root/abort.json"
    printf 'abort exit %s\n' "$?" >>"$case_root/exit-codes.txt"
    set -e
    cp "$case_root/abort.json" "$result"
    wait_terminal "$run_id" "$result" "$label"
  fi
  active_run=
  ended=$(date +%s%3N)

  check passed jq -e '.state == "passed" and .terminal == true and .verdict.overall == "satisfied"' "$result"
  check caller_assisted_false jq -e '.verdict.caller_assisted == false' "$result"
  check steps_satisfied every_step_and_expectation_satisfied "$result" "$job"
  check manifest_complete manifest_is_complete "$result"
  check manifest_covers_journey manifest_covers_journey "$result"

  local manifest trace observation=
  manifest=$(jq -r '.evidence.manifest // empty' "$result")
  trace=$(jq -r '.artifacts[] | select(.role == "trace") | .path' "$manifest" 2>/dev/null || true)
  if [[ -f "$trace" ]]; then
    check hover_sequence hover_sequence_holds "$trace"
    observation=$(observation_before_rent_hover "$trace" "$manifest" || true)
    check observation_before_rent_hover_lists_groceries_actions \
      rent_hover_started_from_groceries_revealed "$observation"
    check no_groceries_click_from_step_two no_groceries_click_from_rent_step "$trace"
  else
    check trace_present false
  fi
  check no_node_ids no_internal_identity_in "$case_root"
  check no_provider_key no_provider_key_in "$case_root"
  check no_leftover_processes no_leftovers "$active_runtime" "$run_id" "$case_root/leftovers.txt"

  local actions='[]' decisions='[]' decision_files=()
  if [[ -f "$trace" ]]; then
    actions=$(jq -s -c '[.[] | select(.event == "action_prepared") |
      if .operation == "HOVER" then "HOVER \(.hover_target.name)" else "\(.operation) \(.target.name // "")" end]' "$trace")
  fi
  if [[ -n "$manifest" && -f "$manifest" ]]; then
    mapfile -t decision_files < <(jq -r '.artifacts[] | select(.role == "decision") | .path' "$manifest")
  fi
  # Each decision shows the target question its chosen operation consumed, not the others.
  if ((${#decision_files[@]})); then
    decisions=$(jq -c -n '[inputs | .request.state.page as $page | {
        decision: (input_filename | split("/") | last),
        goal: .request.state.current_step.goal,
        operation: .operation.choice,
        operation_confidence: .operation.confidence
      } + if .operation.choice == "HOVER" then {
        hover_region: (.hover_target.choice as $key | [$page.hover_regions[]? | select(.key == $key) | .name] | first),
        hover_confidence: .hover_target.confidence
      } elif .operation.choice == "CLICK" then {
        click_target: (.click_target.choice as $key | [$page.elements[]? | select("\(.index)" == $key) | .name] | first),
        click_target_confidence: .click_target.confidence
      } else {} end | with_entries(select(.value != null))]' "${decision_files[@]}")
  fi
  jq -n --arg label "$label" --arg run_id "$run_id" --arg outcome "$outcome" \
    --arg state "$(jq -r '.state' "$result")" \
    --argjson caller_assisted "$(jq '.verdict.caller_assisted' "$result")" \
    --argjson steps "$(jq -c '[.verdict.steps[]? | {id, result}]' "$result")" \
    --argjson actions "$actions" --argjson decisions "$decisions" \
    --arg observation_before_rent_hover "$observation" \
    --argjson checks "$(jq -R -s -c 'split("\n") | map(select(length > 0) | split("\t") | {key: .[0], value: .[1]}) | from_entries' "$checks")" \
    --argjson ok "$run_ok" --argjson wall_ms "$((ended - started))" \
    '{label:$label,run_id:$run_id,outcome:$outcome,state:$state,caller_assisted:$caller_assisted,steps:$steps,actions:$actions,observation_before_rent_hover:$observation_before_rent_hover,decisions:$decisions,checks:$checks,ok:$ok,wall_ms:$wall_ms}' \
    >>"$live_root/matrix.jsonl"
  rm -rf "$active_runtime"
}

for index in $(seq 1 "$runs"); do
  run_case "$index"
done

kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
server_stopped=true
if kill -0 "$server_pid" 2>/dev/null || curl -fsS -o /dev/null "$origin/browser-hover-reveal.html" 2>/dev/null; then
  server_stopped=false
fi
server_pid=
leftover=$(processes_under "$scratch" | sort -u)
jq -n --argjson server_stopped "$server_stopped" --arg leftover "$leftover" \
  '{fixture_server_stopped:$server_stopped,leftover_pids:($leftover | split("\n") | map(select(length > 0)))}' \
  >"$live_root/cleanup.json"

key_clean=true
no_provider_key_in "$live_root" || key_clean=false
identity_clean=true
no_internal_identity_in "$live_root" || identity_clean=false
jq -s --argjson key_clean "$key_clean" --argjson identity_clean "$identity_clean" \
  --slurpfile cleanup "$live_root/cleanup.json" '
  {schema_version:1,journey:"hover_reveal",runs:.,
   passed:([.[] | select(.ok)] | length),total:length,
   provider_key_absent:$key_clean,internal_identity_absent:$identity_clean,cleanup:$cleanup[0]}
' "$live_root/matrix.jsonl" >"$live_root/matrix.json"

echo "$live_root"
jq -e --argjson runs "$runs" '
  .total == $runs and .passed == $runs and .provider_key_absent and .internal_identity_absent and
  .cleanup.fixture_server_stopped and (.cleanup.leftover_pids | length == 0)
' "$live_root/matrix.json" >/dev/null

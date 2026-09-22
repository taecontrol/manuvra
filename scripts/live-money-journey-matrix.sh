#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required for the money journey matrix"
}

epoch_millis() {
  printf '%s000\n' "$(date +%s)"
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

port_is_open() {
  nc -z -w 2 127.0.0.1 4351 >/dev/null 2>&1
}

validate_money_dir() {
  [[ -n "${MONEY_DIR:-}" ]] || fail "MONEY_DIR is required for the money journey matrix"
  [[ -d "$MONEY_DIR" ]] || fail "MONEY_DIR is not a directory: $MONEY_DIR"
  [[ -f "$MONEY_DIR/package.json" ]] || fail "MONEY_DIR has no package.json: $MONEY_DIR"
  [[ -f "$MONEY_DIR/scripts/app-driver.mjs" ]] ||
    fail "MONEY_DIR has no scripts/app-driver.mjs: $MONEY_DIR"
}

preflight_fixture() {
  validate_money_dir
  require_command jq
  require_command nc
  require_command node
  require_command pnpm
  if port_is_open; then
    fail "port 4351 is already in use"
  fi
}

preflight_matrix() {
  preflight_fixture
  require_command awk
  require_command cargo
  require_command date
  require_command git
  require_command rg
  require_command rustc
  require_command shasum
  [[ -n "${TMPDIR:-}" ]] || fail "TMPDIR is required for Darwin headed browser runs"
  [[ -d "$TMPDIR" ]] || fail "TMPDIR is not a directory: $TMPDIR"
  : "${TYPESAFE_API_KEY:?TYPESAFE_API_KEY is required for the money journey matrix}"
}

verification_facts_are_visible() {
  local payload=$1 journey=$2 expected_name=$3
  local run_root snapshot_relative snapshot
  run_root=$(dirname "$(dirname "$payload")")
  snapshot_relative=$(jq -r '.observation.snapshot // empty' "$payload")
  [[ "$snapshot_relative" == observations/* && "$snapshot_relative" != *..* ]]
  snapshot="$run_root/$snapshot_relative"
  [[ -f "$snapshot" ]]
  case "$journey" in
    create-unit)
      jq -e --arg name "$expected_name" \
        '.visible_text | contains($name) and contains("USD")' "$snapshot" >/dev/null
      ;;
    create-account|forced-escalation)
      jq -e --arg name "$expected_name" \
        '.visible_text | contains($name) and contains("12.34")' "$snapshot" >/dev/null
      ;;
    record-transaction)
      jq -e --arg name "$expected_name" '
        .visible_text | contains($name) and contains("Review income") and
        contains("5.66") and contains("18.00")
      ' "$snapshot" >/dev/null
      ;;
    *)
      return 1
      ;;
  esac
}

self_test_portable_helpers() {
  local test_root payload snapshot rows build report linux_report milliseconds
  test_root=$(mktemp -d)
  trap 'rm -rf -- "$test_root"' RETURN
  mkdir -p "$test_root/escalations" "$test_root/observations"
  payload="$test_root/escalations/payload.json"
  snapshot="$test_root/observations/final.json"
  printf '%s\n' '{"observation":{"snapshot":"observations/final.json"}}' >"$payload"

  printf '%s\n' '{"visible_text":"Review wallet 12.34 USD"}' >"$snapshot"
  verification_facts_are_visible "$payload" forced-escalation "Review wallet"

  printf '%s\n' '{"visible_text":"Review wallet USD"}' >"$snapshot"
  if verification_facts_are_visible "$payload" forced-escalation "Review wallet"; then
    echo "forced escalation attestation accepted a snapshot without the balance" >&2
    return 1
  fi

  printf '%s\n' '{"visible_text":"Other wallet 12.34 USD"}' >"$snapshot"
  if verification_facts_are_visible "$payload" forced-escalation "Review wallet"; then
    echo "forced escalation attestation accepted a snapshot without the account name" >&2
    return 1
  fi

  printf 'portable digest fixture\n' >"$test_root/digest"
  [[ "$(sha256_file "$test_root/digest")" == \
    "3bcb080d1c31c3ce588420877f62ccd50f67f5cfcb577464edb084b4bde6d176" ]]
  milliseconds=$(epoch_millis)
  [[ "$milliseconds" != *[!0-9]* && ${#milliseconds} -ge 13 ]]

  build="$test_root/build.json"
  rows="$test_root/runs.jsonl"
  report="$test_root/report.json"
  printf '%s\n' '{"source_revision":"fixture","sha256":"fixture"}' >"$build"
  printf '%s\n' '{"classification":"autonomous","checks":{"fixture":true}}' >"$rows"
  write_report "$build" "$rows" "$report" "$BASH_VERSION"
  jq -e --arg bash_version "$BASH_VERSION" \
    '.bash_version == $bash_version and .counts.autonomous == 1 and .checks.every_run_integrity_persistence_cleanup_and_leaks' \
    "$report" >/dev/null
  linux_report="$test_root/linux-report.json"
  write_report "$build" "$rows" "$linux_report" "5.2.0(1)-release"
  jq -e '.bash_version == "5.2.0(1)-release"' "$linux_report" >/dev/null
}

active_fixture=
active_fixture_state=
fixture_test_root=
cleanup_active_fixture() {
  if [[ -n "$active_fixture" ]]; then
    (cd "$money_dir" && VERIFY_STATE="$active_fixture_state" \
      pnpm verify:app cleanup --run-id "$active_fixture") >/dev/null || true
  fi
}

cleanup_fixture_self_test() {
  cleanup_active_fixture
  if [[ -n "$fixture_test_root" ]]; then
    rm -rf -- "$fixture_test_root"
  fi
}

start_fixture() {
  local run_id=$1 fixture_state=$2 launch=$3 doctor=$4 candidate
  active_fixture=$run_id
  active_fixture_state=$fixture_state
  (cd "$money_dir" && VERIFY_STATE="$fixture_state" \
    pnpm verify:app launch --run-id "$run_id" --port 4351) >"$launch"
  candidate=$(jq -r '.result.candidate // empty' "$launch")
  [[ -n "$candidate" ]] || fail "fixture launch did not publish a candidate"
  (cd "$money_dir" && VERIFY_STATE="$fixture_state" node scripts/app-driver.mjs doctor \
    --run-id "$run_id" --candidate "$candidate") >"$doctor"
  jq -e '.status == "completed"' "$doctor" >/dev/null || fail "fixture doctor failed"
}

stop_fixture() {
  local output=$1 errors=$2 code
  set +e
  (cd "$money_dir" && VERIFY_STATE="$active_fixture_state" \
    pnpm verify:app cleanup --run-id "$active_fixture") >"$output" 2>"$errors"
  code=$?
  set -e
  [[ $code -eq 0 ]] || return "$code"
  jq -e '.status == "completed" and .result.cleanup == "cleaned"' "$output" >/dev/null || return 1
  active_fixture=
  active_fixture_state=
}

self_test_fixture() {
  local run_id
  preflight_fixture
  fixture_test_root=$(mktemp -d)
  fixture_test_root=$(cd "$fixture_test_root" && pwd -P)
  trap cleanup_fixture_self_test EXIT
  run_id="manuvra-money-preflight-$(date +%Y%m%d-%H%M%S)-$$"
  start_fixture "$run_id" "$fixture_test_root/state" "$fixture_test_root/launch.json" \
    "$fixture_test_root/doctor.json" >/dev/null
  stop_fixture "$fixture_test_root/cleanup.json" "$fixture_test_root/cleanup.stderr"
  if port_is_open; then
    fail "fixture self-test left port 4351 open"
  fi
}

write_report() {
  local build=$1 rows=$2 destination=$3 bash_version=$4
  jq -s \
    --slurpfile build "$build" \
    --arg bash_version "$bash_version" \
    '{schema_version:1,bash_version:$bash_version,build:$build[0],runs:.,
      counts:((group_by(.classification) | map({key:.[0].classification,value:length}) | from_entries) +
        {autonomous:([.[]|select(.classification=="autonomous")]|length),
         assisted:([.[]|select(.classification=="assisted")]|length),
         failed:([.[]|select(.classification=="failed")]|length)}),
      checks:{release_binary:true,fresh_fixture_per_run:true,
        every_run_integrity_persistence_cleanup_and_leaks:([.[].checks[]]|all)}}' \
    "$rows" >"$destination"
}

if [[ ${1:-} == --self-test ]]; then
  self_test_portable_helpers
  exit 0
fi

if [[ ${1:-} == --fixture-self-test ]]; then
  money_dir=${MONEY_DIR:-}
  self_test_fixture
  exit 0
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-}
preflight_matrix
runtime_root=$TMPDIR
unset XDG_RUNTIME_DIR

stamp=$(date +%Y%m%d-%H%M%S)-$$
matrix_root="$repo_root/.work/live/money-journey/$stamp"
report_rows="$matrix_root/runs.jsonl"
mkdir -p "$matrix_root"
trap cleanup_active_fixture EXIT

cargo build --release --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
manuvra="$repo_root/target/release/manuvra"
binary_digest=$(sha256_file "$manuvra")
source_revision=$(git -C "$repo_root" rev-parse HEAD)
manuvra_version=$($manuvra version | jq -r '.version')
jq -n \
  --arg binary "$manuvra" \
  --arg sha256 "$binary_digest" \
  --arg source_revision "$source_revision" \
  --arg version "$manuvra_version" \
  --arg rustc "$(rustc --version)" \
  --arg bash_version "$BASH_VERSION" \
  --arg tmpdir "$TMPDIR" \
  '{binary:$binary,sha256:$sha256,source_revision:$source_revision,version:$version,
    rustc:$rustc,bash_version:$bash_version,tmpdir:$tmpdir,xdg_runtime_dir:null}' \
  >"$matrix_root/build.json"

wait_checkpoint() {
  local state_root=$1 run_id=$2 current=$3
  local state code next
  state=$(jq -r '.state' "$current")
  for attempt in $(seq 1 24); do
    [[ "$state" == running ]] || return 0
    next="${current%.json}-status-$attempt.json"
    set +e
    XDG_STATE_HOME="$state_root" \
      "$manuvra" status "$run_id" --wait-ms 30000 >"$next" 2>"${next%.json}.stderr"
    code=$?
    set -e
    [[ $code -eq 0 || $code -eq 2 || $code -eq 3 || $code -eq 4 || $code -eq 5 || $code -eq 6 ]]
    cp "$next" "$current"
    state=$(jq -r '.state' "$current")
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
    (.step_id == "submit" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "open-account-dialog" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "account-name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Account name" and $c.value_name == "account_name") or
    (.step_id == "open-unit-list" and $c.operation == "CLICK" and $c.target_name == "Currency or asset") or
    (.step_id == "open-unit-dialog" and $c.operation == "CLICK" and $c.target_name == "+ New currency or asset") or
    (.step_id == "unit-symbol" and $c.operation == "TYPE_TEXT" and $c.target_name == "Symbol" and $c.value_name == "unit_symbol") or
    (.step_id == "unit-name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Unit name" and $c.value_name == "unit_name") or
    (.step_id == "opening-balance" and $c.operation == "TYPE_TEXT" and $c.target_name == "Opening balance" and $c.value_name == "opening_balance") or
    (.step_id == "create-account" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "open-account" and $c.operation == "CLICK" and $c.target_name == "Transaction wallet") or
    (.step_id == "open-transaction" and $c.operation == "CLICK" and $c.target_name == "Add transaction") or
    (.step_id == "transaction-amount" and $c.operation == "TYPE_TEXT" and $c.target_name == "Amount" and $c.value_name == "transaction_amount") or
    (.step_id == "transaction-note" and $c.operation == "TYPE_TEXT" and $c.target_name == "Note" and $c.value_name == "transaction_note") or
    (.step_id == "save-transaction" and $c.operation == "CLICK" and $c.target_name == "Save")
  ' "$payload" >/dev/null
}

verify_manifest() {
  local result=$1 manifest
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" ]]
    [[ "$(sha256_file "$path")" == "$expected" ]]
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

verify_persistence() {
  local journey=$1 expected_name=$2 observation=$3
  case "$journey" in
    create-unit)
      jq -e --arg name "$expected_name" '
        ([.result.accounts[] | select(.name == $name)] | length) == 1 and
        (.result.accounts | length) == 1 and (.result.units | length) == 1 and
        (.result.operations | length) == 0
      ' "$observation" >/dev/null
      ;;
    create-account|forced-escalation)
      jq -e --arg name "$expected_name" '
        ([.result.accounts[] | select(.name == $name and .opening == "12.34")] | length) == 1 and
        (.result.accounts | length) == 1 and (.result.units | length) == 1 and
        (.result.operations | length) == 0
      ' "$observation" >/dev/null
      ;;
    record-transaction)
      jq -e --arg name "$expected_name" '
        ([.result.accounts[] | select(.name == $name and .opening == "12.34")] | length) == 1 and
        (.result.accounts | length) == 1 and (.result.units | length) == 1 and
        ([.result.operations[] | select(.type == "income" and .amount == "5.66" and .note == "Review income")] | length) == 1 and
        (.result.operations | length) == 1
      ' "$observation" >/dev/null
      ;;
  esac
}

run_case() {
  local journey=$1 iteration=$2 fixture=$3 expected_name=$4 feature=$5 forced=$6
  local label="$journey-$iteration" case_root="$matrix_root/$journey/$iteration"
  local state_root="$case_root/state" evidence_root="$case_root/evidence"
  local job="$fixture" current="$case_root/current-result.json"
  local fixture_state="$case_root/money-fixture"
  mkdir -p "$case_root" "$state_root" "$evidence_root"

  active_fixture="manuvra-money-$journey-$iteration-$stamp"
  local launch="$case_root/launch.json"
  start_fixture "$active_fixture" "$fixture_state" "$launch" "$case_root/doctor.json"

  if [[ "$forced" == yes ]]; then
    job="$case_root/job.json"
    jq '.options.pause_timeout_ms=120000 | .options.lifetime_ms=300000' "$fixture" >"$job"
  fi

  local request_id="money-$journey-$iteration-$stamp" code started_ms ended_ms run_id state
  local assists=0 attestations=0 first_stop= first_stop_payload= forced_stop_seen=false failure=
  local manifest_ok=false persistence_ok=false cleanup_ok=false leak_free=false
  started_ms=$(epoch_millis)
  set +e
  XDG_STATE_HOME="$state_root" \
    "$manuvra" run --request-id "$request_id" --job "$job" --evidence "$evidence_root" \
    --wait-ms 30000 >"$current" 2>"$case_root/initial-result.stderr"
  code=$?
  set -e
  cp "$current" "$case_root/initial-result.json"
  if [[ $code -ne 0 && $code -ne 2 && $code -ne 6 ]]; then
    failure="run exited $code"
  fi
  run_id=$(jq -r '.run_id // empty' "$current")
  if [[ -z "$failure" && -n "$run_id" ]]; then
    wait_checkpoint "$state_root" "$run_id" "$current" || failure="run did not reach a checkpoint"
  fi

  while [[ -z "$failure" ]]; do
    state=$(jq -r '.state' "$current")
    [[ "$state" == uncertain ]] || break
    assists=$((assists + 1))
    if (( assists > 24 )); then
      failure="more than 24 dispositions were required"
      break
    fi

    local escalation_id phase payload disposition resume_result resume_code
    escalation_id=$(jq -r '.escalation.id // empty' "$current")
    phase=$(jq -r '.escalation.phase // empty' "$current")
    payload=$(jq -r '.escalation.payload // empty' "$current")
    if [[ -z "$first_stop" ]]; then
      first_stop="$case_root/first-stop"
      mkdir -p "$first_stop"
      cp "$current" "$first_stop/result.json"
      if [[ -f "$payload" ]]; then
        cp "$payload" "$first_stop/payload.json"
        first_stop_payload="$first_stop/payload.json"
      fi
    fi
    if [[ $(jq -r '.reason.code // empty' "$current") == debug_forced_stop ]]; then
      if [[ $(jq -r '.escalation.step_id // empty' "$current") == submit ]]; then
        forced_stop_seen=true
      fi
    fi

    disposition="$case_root/disposition-$assists.json"
    if [[ "$phase" == verification ]] && jq -e '.escalation.dispositions | index("advance") != null' "$current" >/dev/null; then
      if [[ ! -f "$payload" ]] || ! verification_facts_are_visible "$payload" "$journey" "$expected_name"; then
        failure="verification escalation did not visibly support caller attestation"
        break
      fi
      jq -n --arg escalation "$escalation_id" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"advance",rationale:"The escalation snapshot visibly contains every expected final fact; persistence is checked independently."}}' \
        >"$disposition"
      attestations=$((attestations + 1))
    elif [[ -f "$payload" ]] && jq -e '.offered_candidate != null' "$payload" >/dev/null &&
      jq -e '.escalation.dispositions | index("execute") != null' "$current" >/dev/null; then
      if ! candidate_is_expected "$payload"; then
        failure="escalation offered a candidate that does not match the job"
        break
      fi
      jq -n --arg escalation "$escalation_id" \
        --arg candidate_id "$(jq -r '.offered_candidate.id' "$payload")" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"execute",candidate_id:$candidate_id}}' \
        >"$disposition"
    elif jq -e '.escalation.dispositions | index("retry_observation") != null' "$current" >/dev/null; then
      jq -n --arg escalation "$escalation_id" \
        '{schema_version:1,escalation_id:$escalation,disposition:{kind:"retry_observation"}}' \
        >"$disposition"
    else
      failure="no safe offered disposition could continue the run"
      break
    fi

    resume_result="$case_root/resume-$assists.json"
    set +e
    XDG_STATE_HOME="$state_root" \
      "$manuvra" resume "$run_id" \
      --request-id "$request_id-resume-$assists" --input "$disposition" \
      >"$resume_result" 2>"${resume_result%.json}.stderr"
    resume_code=$?
    set -e
    if [[ $resume_code -ne 0 && $resume_code -ne 2 && $resume_code -ne 6 ]]; then
      failure="resume $assists exited $resume_code"
      cp "$resume_result" "$current"
      break
    fi
    cp "$resume_result" "$current"
    wait_checkpoint "$state_root" "$run_id" "$current" || {
      failure="resume $assists did not reach a checkpoint"
      break
    }
  done
  ended_ms=$(epoch_millis)

  state=$(jq -r '.state // "invalid"' "$current")
  if [[ -z "$failure" && "$state" != passed ]]; then
    failure="terminal state was $state"
  fi
  if [[ -z "$failure" ]] && ! jq -e '
    .terminal == true and .evidence.complete == true and .verdict.overall == "satisfied" and
    ([.verdict.steps[].result] | all(. == "satisfied")) and
    ([.verdict.expectations[].result] | all(. == "satisfied"))
  ' "$current" >/dev/null; then
    failure="terminal result was not completely satisfied"
  fi
  if [[ -z "$failure" ]] && (( assists > 0 )) &&
    ! jq -e '.verdict.caller_assisted == true' "$current" >/dev/null; then
    failure="assisted run omitted caller_assisted"
  fi
  if verify_manifest "$current"; then
    manifest_ok=true
  elif [[ -z "$failure" ]]; then
    failure="manifest or artifact digest verification failed"
  fi
  if [[ "$forced" == yes && "$forced_stop_seen" != true && -z "$failure" ]]; then
    failure="forced submit escalation was not observed"
  fi

  local observation="$case_root/persistence.json"
  (cd "$money_dir" && VERIFY_STATE="$fixture_state" node scripts/app-driver.mjs observe \
    --run-id "$active_fixture" --feature "$feature") >"$observation"
  if verify_persistence "$journey" "$expected_name" "$observation"; then
    persistence_ok=true
  elif [[ -z "$failure" ]]; then
    failure="application persistence did not match the journey"
  fi

  local active_ms=0 manifest classification first_stop_json reason
  manifest=$(jq -r '.evidence.manifest // empty' "$current")
  if [[ -f "$manifest" ]]; then
    active_ms=$(jq -s '[.[] | .active_ms // 0] | max // 0' \
      $(jq -r '.artifacts[] | select(.role == "step") | .path' "$manifest"))
  fi
  classification=autonomous
  (( assists == 0 )) || classification=assisted
  [[ -z "$failure" ]] || classification=failed
  reason=$(jq -r '.reason.code // empty' "$current")
  if [[ -n "$first_stop" ]]; then
    first_stop_json=$(jq -n \
      --arg result "$first_stop/result.json" \
      --arg payload "$first_stop_payload" \
      --arg phase "$(jq -r '.escalation.phase // empty' "$first_stop/result.json")" \
      --arg reason "$(jq -r '.reason.code // empty' "$first_stop/result.json")" \
      --argjson dispositions "$(jq '.escalation.dispositions // []' "$first_stop/result.json")" \
      '{result:$result,payload:(if $payload == "" then null else $payload end),
        phase:(if $phase == "" then null else $phase end),
        reason:(if $reason == "" then null else $reason end),dispositions:$dispositions}')
  else
    first_stop_json=null
  fi

  local cleanup_code=0 port_closed=false
  stop_fixture "$case_root/cleanup.json" "$case_root/cleanup.stderr" || cleanup_code=$?
  if ! port_is_open; then
    port_closed=true
  fi
  if [[ $cleanup_code -eq 0 && "$port_closed" == true ]] &&
    jq -e '.status == "completed" and .result.cleanup == "cleaned"' "$case_root/cleanup.json" >/dev/null; then
    cleanup_ok=true
  elif [[ -z "$failure" ]]; then
    failure="fixture cleanup was not confirmed"
  fi
  [[ -z "$run_id" ]] || rmdir "$runtime_root/manuvra/runs/$run_id" 2>/dev/null || true

  if ! rg -a -l -F -- "$TYPESAFE_API_KEY" "$case_root" >/dev/null &&
    ! rg -a -l '"(document_id|node_id|target_node_id)"' "$case_root" >/dev/null; then
    leak_free=true
  elif [[ -z "$failure" ]]; then
    failure="sensitive provider or browser identity leaked into exported artifacts"
  fi

  classification=autonomous
  (( assists == 0 )) || classification=assisted
  [[ -z "$failure" ]] || classification=failed

  jq -n \
    --arg journey "$journey" --argjson iteration "$iteration" --arg run_id "$run_id" \
    --arg classification "$classification" --arg state "$state" --arg reason "$reason" \
    --arg failure "$failure" --argjson assists "$assists" --argjson active_ms "$active_ms" \
    --argjson wall_ms "$((ended_ms-started_ms))" --arg binary_sha256 "$binary_digest" \
    --argjson first_stop "$first_stop_json" --arg job "$job" --arg result "$current" \
    --arg manifest "$manifest" --arg persistence "$observation" \
    --arg fixture_launch "$launch" --arg fixture_doctor "$case_root/doctor.json" \
    --arg cleanup "$case_root/cleanup.json" --argjson manifest_ok "$manifest_ok" \
    --argjson persistence_ok "$persistence_ok" --argjson cleanup_ok "$cleanup_ok" \
    --argjson leak_free "$leak_free" --argjson attestations "$attestations" \
    '{journey:$journey,iteration:$iteration,run_id:$run_id,classification:$classification,state:$state,
      reason:(if $reason == "" then null else $reason end),failure:(if $failure == "" then null else $failure end),
      assists:$assists,attestations:$attestations,active_ms:$active_ms,wall_ms:$wall_ms,
      binary_sha256:$binary_sha256,job:$job,result:$result,
      evidence_manifest:(if $manifest == "" then null else $manifest end),persistence:$persistence,
      fixture:{launch:$fixture_launch,doctor:$fixture_doctor,cleanup:$cleanup},first_stop:$first_stop,
      checks:{manifest_and_digests:$manifest_ok,persistence:$persistence_ok,
        exported_artifacts_leak_free:$leak_free,cleanup:$cleanup_ok}}' \
    >>"$report_rows"
}

for iteration in 1 2 3; do
  run_case create-unit "$iteration" "$repo_root/tests/live/create-unit.json" \
    "Unit seed wallet" accounts.create no
done
for iteration in 1 2 3; do
  run_case create-account "$iteration" "$repo_root/tests/live/create-account.json" \
    "Review wallet" accounts.create no
done
for iteration in 1 2 3; do
  run_case record-transaction "$iteration" "$repo_root/tests/live/record-transaction.json" \
    "Transaction wallet" history.persistence no
done
run_case forced-escalation 1 "$repo_root/tests/live/create-account-forced-pause.json" \
  "Review wallet" accounts.create yes

write_report "$matrix_root/build.json" "$report_rows" "$matrix_root/report.json" "$BASH_VERSION"

if rg -a -l -F -- "$TYPESAFE_API_KEY" "$matrix_root"; then
  echo "provider key leaked into matrix evidence" >&2
  exit 1
fi
if rg -a -l '"(document_id|node_id|target_node_id)"' "$matrix_root"; then
  echo "internal browser identity leaked into matrix evidence" >&2
  exit 1
fi
if jq -e '.counts.failed // 0 | . > 0' "$matrix_root/report.json" >/dev/null; then
  echo "money journey matrix contains failed runs: $matrix_root/report.json" >&2
  exit 1
fi

echo "$matrix_root"

#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

epoch_millis() {
  printf '%s000\n' "$(date +%s)"
}

preflight_matrix() {
  preflight_fixture
  require_command awk
  require_command cargo
  require_command date
  require_command git
  require_command grep
  require_command rg
  require_command rustc
  require_command shasum
  require_provider_key
}

configure_runtime_root() {
  case "$1" in
    Linux)
      [[ -n "${XDG_RUNTIME_DIR:-}" ]] || fail "XDG_RUNTIME_DIR is required for Linux background runs"
      [[ -d "$XDG_RUNTIME_DIR" ]] || fail "XDG_RUNTIME_DIR is not a directory: $XDG_RUNTIME_DIR"
      runtime_root=$XDG_RUNTIME_DIR
      ;;
    Darwin)
      [[ -n "${TMPDIR:-}" ]] || fail "TMPDIR is required for the macOS runtime fallback"
      [[ -d "$TMPDIR" ]] || fail "TMPDIR is not a directory: $TMPDIR"
      runtime_root=$TMPDIR
      unset XDG_RUNTIME_DIR
      ;;
    *) fail "unsupported platform for the money journey matrix: $1" ;;
  esac
}

self_test_runtime_root() {
  local test_root
  test_root=$(mktemp -d)
  trap 'rm -rf -- "$test_root"' RETURN
  (XDG_RUNTIME_DIR="$test_root"; unset TMPDIR; configure_runtime_root Linux;
    [[ "$runtime_root" == "$test_root" && "$XDG_RUNTIME_DIR" == "$test_root" ]])
  if (unset XDG_RUNTIME_DIR; configure_runtime_root Linux) >/dev/null 2>&1; then
    echo "Linux runtime preflight accepted missing XDG_RUNTIME_DIR" >&2
    return 1
  fi
  (TMPDIR="$test_root" XDG_RUNTIME_DIR="$test_root/xdg";
    configure_runtime_root Darwin;
    [[ "$runtime_root" == "$test_root" && -z "${XDG_RUNTIME_DIR+x}" ]])
  if (unset TMPDIR; configure_runtime_root Darwin) >/dev/null 2>&1; then
    echo "Darwin runtime preflight accepted missing TMPDIR" >&2
    return 1
  fi
}

verification_facts_are_visible() {
  local payload=$1 journey=$2 expected_name=$3
  local run_root snapshot_relative snapshot
  run_root=$(dirname "$(dirname "$payload")")
  snapshot_relative=$(jq -r '.observation.snapshot // empty' "$payload")
  [[ "$snapshot_relative" == observations/* && "$snapshot_relative" != *..* ]] || return 1
  snapshot="$run_root/$snapshot_relative"
  [[ -f "$snapshot" ]] || return 1
  case "$journey" in
    create-unit)
      jq -e --arg name "$expected_name" \
        '.visible_text | contains($name) and contains("USD")' "$snapshot" >/dev/null
      ;;
    create-account|forced-escalation)
      jq -e --arg name "$expected_name" \
        '.visible_text | contains($name) and contains("12.34")' "$snapshot" >/dev/null
      ;;
    create-account-secret)
      # Evidence masks the classified name, so the snapshot shows its marker instead.
      jq -e --arg name "$expected_name" '
        .visible_text | contains("12.34") and (contains($name) | not) and
        test("<masked:[0-9]+>|<value:account_name>")
      ' "$snapshot" >/dev/null
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

  printf '%s\n' '{"visible_text":"<masked:1> 12.34 USD"}' >"$snapshot"
  verification_facts_are_visible "$payload" create-account-secret "Secret review wallet 7491"

  printf '%s\n' '{"visible_text":"Secret review wallet 7491 12.34 USD"}' >"$snapshot"
  if verification_facts_are_visible "$payload" create-account-secret "Secret review wallet 7491"; then
    echo "secret attestation accepted a snapshot that exposes the classified name" >&2
    return 1
  fi

  printf '%s\n' '{"step_id":"open","offered_candidate":{"operation":"CLICK","target_name":"+ Create account"}}' \
    >"$test_root/candidate.json"
  candidate_is_expected "$test_root/candidate.json"
  printf '%s\n' '{"step_id":"open","offered_candidate":{"operation":"CLICK","target_name":"Delete account"}}' \
    >"$test_root/candidate.json"
  if candidate_is_expected "$test_root/candidate.json"; then
    echo "candidate check accepted a target the job does not intend" >&2
    return 1
  fi

  self_test_provider_key_scan "$test_root/scan"

  printf 'portable digest fixture\n' >"$test_root/digest"
  [[ "$(sha256_file "$test_root/digest")" == \
    "91668ee0646c7b6712ff1a925b0a02d9d29037461f936f7999d7ac8cab683c18" ]]
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

# Uses a synthetic key; the real provider key is never read here.
self_test_provider_key_scan() {
  local root=$1
  mkdir -p "$root/clean" "$root/leaked"
  printf 'redacted <masked-provider>\n' >"$root/clean/evidence.txt"
  printf 'header manuvra-self-test-key trailer\n' >"$root/leaked/evidence.txt"
  (
    export TYPESAFE_API_KEY=manuvra-self-test-key
    if provider_key_present "$root/clean" >/dev/null; then
      fail "provider key scan reported a key in clean evidence"
    fi
    provider_key_present "$root/clean" "$root/leaked" >/dev/null ||
      fail "provider key scan missed a leaked key"
    provider_key_present "$root/missing" >/dev/null 2>&1 ||
      fail "provider key scan treated an unreadable path as clean"
    unset TYPESAFE_API_KEY
    provider_key_present "$root/clean" >/dev/null 2>&1 ||
      fail "provider key scan treated a missing key as absent"
  )
}

fixture_test_root=
cleanup_fixture_self_test() {
  cleanup_active_fixture
  if [[ -n "$fixture_test_root" ]]; then
    rm -rf -- "$fixture_test_root"
  fi
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

if [[ ${1:-} == --runtime-self-test ]]; then
  self_test_runtime_root
  exit 0
fi

if [[ ${1:-} == --fixture-self-test ]]; then
  self_test_fixture
  exit 0
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
preflight_matrix
configure_runtime_root "$(uname -s)"

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
  --arg tmpdir "${TMPDIR:-}" \
  --arg xdg_runtime_dir "${XDG_RUNTIME_DIR:-}" \
  '{binary:$binary,sha256:$sha256,source_revision:$source_revision,version:$version,
    rustc:$rustc,bash_version:$bash_version,tmpdir:(if $tmpdir == "" then null else $tmpdir end),
    xdg_runtime_dir:(if $xdg_runtime_dir == "" then null else $xdg_runtime_dir end)}' \
  >"$matrix_root/build.json"

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
    create-account|create-account-secret|forced-escalation)
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

# A job with final expectations publishes one verification Artifact that satisfies every one.
final_verification_is_satisfied() {
  local job=$1 result=$2 manifest verification
  jq -e '.expectations | length > 0' "$job" >/dev/null || return 0
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '[.artifacts[] | select(.role == "verification")] | length == 1' "$manifest" >/dev/null ||
    return 1
  verification=$(jq -r '.artifacts[] | select(.role == "verification") | .path' "$manifest")
  jq -e '.phase == "verification" and ([.expectations[].result] | all(. == "satisfied"))' \
    "$verification" >/dev/null
}

# The classified account name never reaches Manuvra's results, state, or Evidence. The final
# verification asks Jev about the named value, and no masked fragment survives as a literal.
classified_name_is_redacted() {
  local name=$1 state_root=$2 manifest run_dir verification status=0
  shift 2
  manifest=$(jq -r '.evidence.manifest // empty' "$1")
  [[ -f "$manifest" ]] || return 1
  run_dir=$(dirname "$manifest")
  verification=$(jq -r '.artifacts[] | select(.role == "verification") | .path' "$manifest")
  [[ -f "$verification" ]] || return 1
  grep -r -a -q -F -- "$name" "$run_dir" "$state_root" "$@" || status=$?
  [[ $status -eq 1 ]] || return 1
  jq -e '.provider.request | tostring | contains("<value:account_name>")' "$verification" \
    >/dev/null || return 1
  if jq -e --arg marker "${name##* }" '.. | objects | .literal? // empty | select(. == $marker)' \
    "$verification" "$1" >/dev/null; then
    return 1
  fi
  grep -r -a -q -E '<masked:[0-9]+>|<value:account_name>' "$run_dir"
}

run_case() {
  local journey=$1 iteration=$2 fixture=$3 expected_name=$4 feature=$5 forced=$6
  local case_root="$matrix_root/$journey/$iteration"
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
  local assists=0 attestations=0 first_stop='' first_stop_payload='' forced_stop_seen=false failure=''
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
  if [[ -z "$failure" ]] && ! final_verification_is_satisfied "$job" "$current"; then
    manifest_ok=false
    failure="final verification Evidence did not satisfy every expectation"
  fi
  if [[ "$forced" == yes && "$forced_stop_seen" != true && -z "$failure" ]]; then
    failure="forced submit escalation was not observed"
  fi

  local observation="$case_root/persistence.json"
  observe_fixture "$feature" >"$observation"
  if verify_persistence "$journey" "$expected_name" "$observation"; then
    persistence_ok=true
  elif [[ -z "$failure" ]]; then
    failure="application persistence did not match the journey"
  fi

  local active_ms=0 manifest classification first_stop_json reason step_path step_files=()
  manifest=$(jq -r '.evidence.manifest // empty' "$current")
  if [[ -f "$manifest" ]]; then
    while IFS= read -r step_path; do
      step_files+=("$step_path")
    done < <(jq -r '.artifacts[] | select(.role == "step") | .path' "$manifest")
  fi
  if (( ${#step_files[@]} > 0 )); then
    active_ms=$(jq -s '[.[] | .active_ms // 0] | max // 0' "${step_files[@]}")
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

  if ! provider_key_present "$case_root" >/dev/null &&
    ! rg -a -l '"(document_id|node_id|target_node_id)"' "$case_root" >/dev/null &&
    { [[ "$journey" != create-account-secret ]] ||
      classified_name_is_redacted "$expected_name" "$state_root" "$current" \
        "$case_root/initial-result.json"; }; then
    leak_free=true
  elif [[ -z "$failure" ]]; then
    failure="sensitive provider, classified value, or browser identity leaked into exported artifacts"
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
  run_case create-unit "$iteration" "$repo_root/tests/live/money/create-unit.json" \
    "Unit seed wallet" accounts.create no
done
for iteration in 1 2 3; do
  run_case create-account "$iteration" "$repo_root/tests/live/money/create-account.json" \
    "Review wallet" accounts.create no
done
for iteration in 1 2 3; do
  run_case record-transaction "$iteration" "$repo_root/tests/live/money/record-transaction.json" \
    "Transaction wallet" history.persistence no
done
run_case create-account-secret 1 "$repo_root/tests/live/money/create-account-secret.json" \
  "Secret review wallet 7491" accounts.create no
run_case forced-escalation 1 "$repo_root/tests/live/money/create-account-forced-pause.json" \
  "Review wallet" accounts.create yes

write_report "$matrix_root/build.json" "$report_rows" "$matrix_root/report.json" "$BASH_VERSION"

if provider_key_present "$matrix_root"; then
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

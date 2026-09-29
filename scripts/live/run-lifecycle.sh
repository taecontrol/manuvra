#!/usr/bin/env bash
# Follows one forced-pause Run through its lifecycle on Linux: the host and Chromium stay alive
# while the Run waits, an identical `run` attaches to it, the resume deadline expires it, a retry
# returns the expired result, and every host, watchdog, and browser process exits.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
preflight_fixture
require_command cargo
require_command pgrep
require_command shasum
require_provider_key
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/run-lifecycle/$stamp"
state_root="$evidence_root/state"
runtime_root=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required for background runs}
mkdir -p "$evidence_root" "$state_root"
trap cleanup_active_fixture EXIT

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
start_fixture "manuvra-run-lifecycle-$stamp" "$evidence_root/money-fixture" \
  "$evidence_root/launch.json" "$evidence_root/doctor.json"

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" run \
  --request-id "run-lifecycle-$stamp" \
  --job "$repo_root/tests/live/money/create-account-forced-pause.json" \
  --evidence "$evidence_root/run" >"$evidence_root/run.json"
run_status=$?
set -e
[[ $run_status -eq 2 || $run_status -eq 6 ]]
run_id=$(jq -r '.run_id' "$evidence_root/run.json")
host_pid=$(jq -r '.host.pid' "$state_root/manuvra/runs/$run_id/control.json")
initial_browser_pid=$(pgrep -P "$host_pid" chromium | head -1)

progress=0
state=$(jq -r '.state' "$evidence_root/run.json")
stop_json="$evidence_root/run.json"
stop_status=$run_status
while [[ "$state" == running ]]; do
  progress=$((progress + 1))
  [[ $progress -le 16 ]]
  stop_json="$evidence_root/status-progress-$progress.json"
  set +e
  XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
    "$repo_root/target/debug/manuvra" status "$run_id" --wait-ms 30000 >"$stop_json"
  stop_status=$?
  set -e
  [[ $stop_status -eq 2 || $stop_status -eq 6 ]]
  state=$(jq -r '.state' "$stop_json")
done
[[ $stop_status -eq 2 ]]
jq -e '.state == "uncertain" and .terminal == false and .reason.code == "debug_forced_stop" and .cleanup.browser == "alive"' \
  "$stop_json" >/dev/null
jq -r '.host.pid' "$state_root/manuvra/runs/$run_id/control.json" | grep -Fx "$host_pid" >/dev/null
pgrep -P "$host_pid" chromium | grep -Fx "$initial_browser_pid" >/dev/null

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" run \
  --request-id "run-lifecycle-$stamp" \
  --job "$repo_root/tests/live/money/create-account-forced-pause.json" \
  --evidence "$evidence_root/run" \
  --wait-ms 0 >"$evidence_root/attach-identical.json"
attach_status=$?
set -e
[[ $attach_status -eq 2 ]]
jq -e --arg run_id "$run_id" \
  '.run_id == $run_id and .state == "uncertain" and .terminal == false and .cleanup.browser == "alive"' \
  "$evidence_root/attach-identical.json" >/dev/null
jq -r '.host.pid' "$state_root/manuvra/runs/$run_id/control.json" | grep -Fx "$host_pid" >/dev/null
pgrep -P "$host_pid" chromium | grep -Fx "$initial_browser_pid" >/dev/null

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" status "$run_id" >"$evidence_root/status-alive.json"
alive_status=$?
set -e
[[ $alive_status -eq 2 ]]
jq -e '.state == "uncertain" and .terminal == false and .cleanup.browser == "alive"' \
  "$evidence_root/status-alive.json" >/dev/null
watchdog_pid=$(jq -r '.watchdog.pid' "$state_root/manuvra/runs/$run_id/control.json")
host_pgid=$(ps -o pgid= -p "$host_pid" | tr -d ' ')
ps -eo pid=,pgid= | awk -v pgid="$host_pgid" '$2 == pgid { print $1 }' \
  >"$evidence_root/host-group-pids.txt"
printf '%s\n' "$watchdog_pid" >>"$evidence_root/lifecycle-pids.txt"
cat "$evidence_root/host-group-pids.txt" >>"$evidence_root/lifecycle-pids.txt"
sort -n -u -o "$evidence_root/lifecycle-pids.txt" "$evidence_root/lifecycle-pids.txt"

# A process that exited before its scan cannot hold the key; any other unreadable field counts as
# a leak because absence is not proved.
while read -r pid; do
  for proc_field in environ cmdline; do
    if provider_key_present "/proc/$pid/$proc_field" >/dev/null 2>&1 && [[ -e "/proc/$pid" ]]; then
      echo "provider key leaked into process $pid $proc_field" >&2
      exit 1
    fi
  done
done <"$evidence_root/lifecycle-pids.txt"

deadline=$(( $(date +%s) + 15 ))
while :; do
  set +e
  XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
    "$repo_root/target/debug/manuvra" status "$run_id" >"$evidence_root/status-expired.json"
  status_code=$?
  set -e
  state=$(jq -r '.state' "$evidence_root/status-expired.json")
  if [[ "$state" == expired ]]; then break; fi
  [[ $(date +%s) -lt $deadline ]]
  sleep 0.1
done
[[ $status_code -eq 5 ]]
jq -e '.terminal == true and .reason.code == "resume_deadline_elapsed" and .cleanup.browser == "closed"' \
  "$evidence_root/status-expired.json" >/dev/null
verify_manifest "$evidence_root/status-expired.json"

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" run \
  --request-id "run-lifecycle-$stamp" \
  --job "$repo_root/tests/live/money/create-account-forced-pause.json" \
  --evidence "$evidence_root/run" >"$evidence_root/retry-after-expiry.json"
retry_status=$?
set -e
[[ $retry_status -eq 5 ]]
jq -e --arg run_id "$run_id" \
  '.run_id == $run_id and .state == "expired" and .terminal == true and .reason.code == "resume_deadline_elapsed" and .evidence.complete == true' \
  "$evidence_root/retry-after-expiry.json" >/dev/null
[[ $(find "$evidence_root/run" -mindepth 1 -maxdepth 1 -type d | wc -l) -eq 1 ]]

process_deadline=$(( $(date +%s) + 2 ))
while read -r pid; do
  while [[ -e "/proc/$pid" && $(date +%s) -lt $process_deadline ]]; do sleep 0.05; done
  [[ ! -e "/proc/$pid" ]]
done <"$evidence_root/lifecycle-pids.txt"

observe_fixture accounts.create >"$evidence_root/observe.json"
jq -e '.result.accounts | length == 0' "$evidence_root/observe.json" >/dev/null
stop_fixture "$evidence_root/cleanup.json" "$evidence_root/cleanup.stderr"
rmdir "$runtime_root/manuvra/runs/$run_id"

if provider_key_present "$evidence_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi
echo "$evidence_root"

#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
money_dir=${MONEY_DIR:-/home/guetteluis/Work/personal/money}
stamp=$(date +%Y%m%d-%H%M%S)-$$
evidence_root="$repo_root/.work/live/run-lifecycle/$stamp"
state_root="$evidence_root/state"
runtime_root=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required for the headed Wayland run}
mkdir -p "$evidence_root" "$state_root"

cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
active_run="manuvra-run-lifecycle-$stamp"
cleanup() {
  (cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_run") >/dev/null || true
}
trap cleanup EXIT

launch=$(cd "$money_dir" && pnpm verify:app launch --run-id "$active_run" --port 4351)
candidate=$(jq -r '.result.candidate' <<<"$launch")
(cd "$money_dir" && node scripts/app-driver.mjs doctor \
  --run-id "$active_run" --candidate "$candidate") >"$evidence_root/doctor.json"

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" run \
  --request-id "run-lifecycle-$stamp" \
  --job "$repo_root/tests/live/create-account-forced-pause.json" \
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
  --job "$repo_root/tests/live/create-account-forced-pause.json" \
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

provider_key=${TYPESAFE_API_KEY-}
if [[ -n "$provider_key" ]]; then
  while read -r pid; do
    for proc_field in environ cmdline; do
      if tr '\0' '\n' <"/proc/$pid/$proc_field" | rg -F -q -- "$provider_key"; then
        echo "provider key leaked into process $pid $proc_field" >&2
        exit 1
      fi
    done
  done <"$evidence_root/lifecycle-pids.txt"
fi

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
manifest=$(jq -r '.evidence.manifest' "$evidence_root/status-expired.json")
jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null
while IFS=$'\t' read -r path expected; do
  [[ -f "$path" ]]
  [[ "$(sha256sum "$path" | cut -d' ' -f1)" == "$expected" ]]
done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")

set +e
XDG_STATE_HOME="$state_root" XDG_RUNTIME_DIR="$runtime_root" \
  "$repo_root/target/debug/manuvra" run \
  --request-id "run-lifecycle-$stamp" \
  --job "$repo_root/tests/live/create-account-forced-pause.json" \
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

(cd "$money_dir" && node scripts/app-driver.mjs observe \
  --run-id "$active_run" --feature accounts.create) >"$evidence_root/observe.json"
jq -e '.result.accounts | length == 0' "$evidence_root/observe.json" >/dev/null
(cd "$money_dir" && pnpm verify:app cleanup --run-id "$active_run") >"$evidence_root/cleanup.json"
trap - EXIT
rmdir "$runtime_root/manuvra/runs/$run_id"

if [[ -n "$provider_key" ]] && rg -a -l -F -- "$provider_key" "$evidence_root"; then
  echo "provider key leaked into live evidence" >&2
  exit 1
fi
echo "$evidence_root"

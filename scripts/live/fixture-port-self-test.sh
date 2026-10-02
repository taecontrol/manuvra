#!/usr/bin/env bash
# Exercise launch, job origins and cleanup with default and caller-selected fixture ports.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
test_root=$(mktemp -d)
trap 'rm -rf -- "$test_root"' EXIT
money_dir=$test_root
pnpm() {
  if [[ $2 == launch ]]; then
    printf '%s\n' "$@" >"$test_root/launch-args"
    printf '%s\n' '{"status":"completed","result":{"candidate":"test-candidate"}}'
  else
    printf '%s\n' '{"status":"completed","result":{"cleanup":"cleaned"}}'
  fi
}
node() {
  printf '%s\n' '{"status":"completed"}'
}
nc() {
  printf '%s\n' "$5" >>"$test_root/checked-ports"
  return 1
}
cat >"$test_root/template.json" <<'JSON'
{"target":{"kind":"browser","url":"http://127.0.0.1:4351/accounts?q=4351"},"options":{"allowed_origins":["http://127.0.0.1:4351","https://other.example"]},"values":{"note":{"value":"4351"}}}
JSON
for port in 4351 4354; do
  : >"$test_root/checked-ports"
  if [[ $port == 4351 ]]; then
    unset MONEY_FIXTURE_PORT
    start_fixture probe "$test_root/state" "$test_root/launch.json" "$test_root/doctor.json"
  else
    MONEY_FIXTURE_PORT=$port
    start_fixture probe "$test_root/state" "$test_root/launch.json" "$test_root/doctor.json"
  fi
  [[ $(tail -1 "$test_root/launch-args") == "$port" ]]
  fixture_job "$test_root/template.json" "$test_root/job.json"
  jq -e --arg port "$port" '
    .target.url == "http://127.0.0.1:"+$port+"/accounts?q=4351" and
    .options.allowed_origins == ["http://127.0.0.1:"+$port,"https://other.example"] and
    .values.note.value == "4351"
  ' "$test_root/job.json" >/dev/null
  if [[ $port == 4351 ]]; then cmp "$test_root/template.json" "$test_root/job.json"; fi
  stop_fixture "$test_root/cleanup.json" "$test_root/cleanup.stderr"
  [[ -z "$active_fixture" && -z "$active_fixture_state" ]]
  [[ $(wc -l <"$test_root/checked-ports") -eq 2 ]]
  if rg -q -v "^$port$" "$test_root/checked-ports"; then
    fail "cleanup checked another fixture's port"
  fi
done
printf '%s\n' 'fixture port self-test passed'

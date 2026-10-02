#!/usr/bin/env bash
# Five independent runs at each viewport; every run counts, including stopped runs.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
checker="$repo_root/scripts/live/money-scroll-check.py"
if [[ ${1:-} == --self-test ]]; then
  python3 "$checker" --self-test
  exit 0
fi
require_provider_key
fixture_port=${MONEY_SCROLL_PORT:-${MONEY_FIXTURE_PORT:-4351}}
[[ "$fixture_port" =~ ^[0-9]+$ && "$fixture_port" -ge 1024 && "$fixture_port" -le 65535 ]] || fail "MONEY_SCROLL_PORT must be between 1024 and 65535"
preflight_fixture "$fixture_port"
money_revision=e013e071cbe3da6153c7c306620dd3487dd4f241
[[ "$(git -C "$money_dir" rev-parse HEAD)" == "$money_revision" ]] || fail "Money must be pinned to $money_revision"
python3 "$checker" --self-test
manuvra="$repo_root/target/release/manuvra"
cargo build --release --locked --manifest-path "$repo_root/Cargo.toml" --bin manuvra
stamp=$(date +%Y%m%d-%H%M%S)-$$
live_root="$repo_root/.work/live/money-scroll/$stamp"
mkdir -p "$live_root"
active_run=
active_state=
cleanup() {
  if [[ -n "$active_run" ]]; then
    XDG_STATE_HOME="$active_state" "$manuvra" abort "$active_run" --request-id "cleanup-$stamp" >/dev/null 2>&1 || true
  fi
  cleanup_active_fixture
}
trap cleanup EXIT
jq -n --arg revision "$(git -C "$repo_root" rev-parse HEAD)" \
  --argjson modified "$(test -z "$(git -C "$repo_root" status --porcelain)" && echo false || echo true)" \
  --arg binary "$(sha256_file "$manuvra")" --arg money_revision "$money_revision" \
  --arg job "$(sha256_file "$repo_root/tests/live/money/category-below-fold.json")" \
  --arg browser "$(chromium --version)" \
  '{source_revision:$revision,working_tree_modified:$modified,binary_sha256:$binary,money_revision:$money_revision,job_sha256:$job,browser:$browser,headless:true,width:1280}' \
  >"$live_root/environment.json"
for height in 800 420; do
  for iteration in $(seq 1 5); do
    case_dir="$live_root/$height-$iteration"
    mkdir -p "$case_dir"
    fixture_id="money-scroll-$stamp-$height-$iteration"
    start_fixture "$fixture_id" "$case_dir/fixture" "$case_dir/launch.json" "$case_dir/doctor.json" "$fixture_port"
    # Driver-owned MCP seeding; neither Money's source nor a user's data is changed.
    (cd "$money_dir" && VERIFY_STATE="$active_fixture_state" \
      node "$repo_root/scripts/live/money-scroll-seed.mjs" "$active_fixture") >"$case_dir/seed.json"
    jq -e '.status == "completed"' "$case_dir/seed.json" >/dev/null
    observe_fixture accounts >"$case_dir/before.json"
    jq -e '.status == "completed" and (.result.accounts|length)==1 and (.result.operations|length)==3 and (.result.categoryCatalog.categories|length)==40' \
      "$case_dir/before.json" >/dev/null
    jq --argjson height "$height" --arg port "$fixture_port" \
      '.options.viewport.height=$height | .target.url |= sub(":4351";":"+$port) | .options.allowed_origins |= map(sub(":4351";":"+$port))' \
      "$repo_root/tests/live/money/category-below-fold.json" >"$case_dir/job.json"
    jq -n --argjson height "$height" --argjson iteration "$iteration" \
      '{height:$height,iteration:$iteration}' >"$case_dir/case.json"
    active_state="$case_dir/state"
    set +e
    XDG_STATE_HOME="$active_state" "$manuvra" run --headless --request-id "$fixture_id" \
      --job "$case_dir/job.json" --evidence "$case_dir/evidence" >"$case_dir/result.json" 2>"$case_dir/run.stderr"
    code=$?
    set -e
    [[ $code -eq 0 || $code -eq 2 || $code -eq 3 || $code -eq 4 || $code -eq 5 || $code -eq 6 ]] || fail "Manuvra returned an invalid checkpoint"
    active_run=$(jq -r '.run_id' "$case_dir/result.json")
    wait_checkpoint "$active_state" "$active_run" "$case_dir/result.json"
    observe_fixture accounts >"$case_dir/after.json"
    jq -e '.status == "completed"' "$case_dir/after.json" >/dev/null
    if [[ $(jq -r '.state' "$case_dir/result.json") == uncertain ]]; then
      abort_paused_run "$active_state" "$active_run" "$case_dir/abort.json"
    fi
    python3 "$checker" --case "$case_dir"
    active_run=
    stop_fixture "$case_dir/cleanup.json" "$case_dir/cleanup.stderr"
    touch "$case_dir/cleanup-confirmed"
  done
done
if provider_key_present "$live_root"; then
  fail "provider key absence could not be proven"
fi
touch "$live_root/key-absent-confirmed"
python3 "$checker" --report "$live_root"

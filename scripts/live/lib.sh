# shellcheck shell=bash
# Shared helpers for the live suites. Source this file; it defines functions only.
# It must stay compatible with the macOS system Bash 3.2 because the Money journey matrix uses it.

fail() {
  echo "$1" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required for this live suite"
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

# The provider key never enters an argument list or xtrace output: `printenv` receives only the
# variable name, and the scanners read the key through a file descriptor.
require_provider_key() {
  printenv TYPESAFE_API_KEY | grep -q . ||
    fail "TYPESAFE_API_KEY must be exported for live Jev judgments"
}

# Succeeds when the exported provider key occurs in any file beneath the given paths. Prints only the
# names of matching files. A missing key or a scan error counts as present because absence is not
# proved.
provider_key_present() {
  local status=0
  if ! printenv TYPESAFE_API_KEY >/dev/null; then
    echo "TYPESAFE_API_KEY is not exported; provider key absence cannot be proved" >&2
    return 0
  fi
  grep -r -a -l -F -f <(printenv TYPESAFE_API_KEY) -- "$@" || status=$?
  (( status != 1 ))
}

port_is_open() {
  nc -z -w 2 127.0.0.1 4351 >/dev/null 2>&1
}

validate_money_dir() {
  [[ -n "${MONEY_DIR:-}" ]] || fail "MONEY_DIR must name a disposable Money checkout"
  [[ -d "$MONEY_DIR" ]] || fail "MONEY_DIR is not a directory: $MONEY_DIR"
  [[ -f "$MONEY_DIR/package.json" ]] || fail "MONEY_DIR has no package.json: $MONEY_DIR"
  [[ -f "$MONEY_DIR/scripts/app-driver.mjs" ]] ||
    fail "MONEY_DIR has no scripts/app-driver.mjs: $MONEY_DIR"
  money_dir=$MONEY_DIR
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

# One Money fixture is active at a time. Its state lives in the caller-chosen directory.
active_fixture=
active_fixture_state=

cleanup_active_fixture() {
  if [[ -n "$active_fixture" ]]; then
    (cd "$money_dir" && VERIFY_STATE="$active_fixture_state" \
      pnpm verify:app cleanup --run-id "$active_fixture") >/dev/null || true
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

observe_fixture() {
  local feature=$1
  (cd "$money_dir" && VERIFY_STATE="$active_fixture_state" node scripts/app-driver.mjs observe \
    --run-id "$active_fixture" --feature "$feature")
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
  port_released
}

# Waits for the fixture runtime to release port 4351; cleanup can report before its last worker exits.
port_released() {
  local deadline
  deadline=$(( $(date +%s) + 10 ))
  while port_is_open && [[ $(date +%s) -lt $deadline ]]; do sleep 0.1; done
  ! port_is_open
}

# Follows a running Run with `status` until it reaches a checkpoint. Uses the caller's `$manuvra`.
wait_checkpoint() {
  local state_root=$1 run_id=$2 current=$3
  local state code next attempt
  state=$(jq -r '.state' "$current")
  for attempt in $(seq 1 24); do
    [[ "$state" == running ]] || return 0
    next="${current%.json}-status-$attempt.json"
    set +e
    XDG_STATE_HOME="$state_root" \
      "${manuvra:?wait_checkpoint needs the manuvra binary path}" status "$run_id" \
      --wait-ms 30000 >"$next" 2>"${next%.json}.stderr"
    code=$?
    set -e
    [[ $code -eq 0 || $code -eq 2 || $code -eq 3 || $code -eq 4 || $code -eq 5 || $code -eq 6 ]] ||
      return 1
    cp "$next" "$current"
    state=$(jq -r '.state' "$current")
  done
  [[ "$state" != running ]]
}

# Follows a Run from its first result to the next non-running checkpoint, rewrites `current`
# with that checkpoint, and prints the exit code `status` reports for it.
settle_run() {
  local state_root=$1 current=$2 run_id code
  run_id=$(jq -r '.run_id' "$current")
  wait_checkpoint "$state_root" "$run_id" "$current" || return 1
  set +e
  XDG_STATE_HOME="$state_root" "${manuvra:?settle_run needs the manuvra binary path}" \
    status "$run_id" >"$current.settled" 2>"$current.settled.stderr"
  code=$?
  set -e
  mv "$current.settled" "$current"
  echo "$code"
}

# Aborts a Run paused for a disposition so it releases its browser, and confirms the abort.
abort_paused_run() {
  local state_root=$1 run_id=$2 output=$3 code
  set +e
  XDG_STATE_HOME="$state_root" "${manuvra:?abort_paused_run needs the manuvra binary path}" \
    abort "$run_id" --request-id "abort-$run_id" >"$output" 2>"${output%.json}.stderr"
  code=$?
  set -e
  [[ $code -eq 5 ]] && jq -e '.state == "aborted" and .terminal == true' "$output" >/dev/null
}

# Waits for Manuvra to remove a finished Run's private runtime directory.
runtime_dir_removed() {
  local dir=$1 deadline
  deadline=$(( $(date +%s) + 5 ))
  while [[ -e "$dir" && $(date +%s) -lt $deadline ]]; do sleep 0.05; done
  [[ ! -e "$dir" ]]
}

# Succeeds when an escalation payload offers the one operation each Money job step intends.
candidate_is_expected() {
  local payload=$1
  jq -e '
    .offered_candidate as $c |
    (.step_id == "open" and $c.operation == "CLICK" and $c.target_name == "+ Create account") or
    (.step_id == "name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Account name" and $c.value_name == "account_name") or
    (.step_id == "currency" and $c.operation == "CLICK" and $c.target_name == "Currency or asset") or
    (.step_id == "new-unit" and $c.operation == "CLICK" and $c.target_name == "+ New currency or asset") or
    (.step_id == "symbol" and $c.operation == "TYPE_TEXT" and $c.target_name == "Symbol" and $c.value_name == "unit_symbol") or
    (.step_id == "unit-name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Unit name" and $c.value_name == "unit_name") or
    (.step_id == "use-unit" and $c.operation == "CLICK" and $c.target_name == "Use unit") or
    (.step_id == "balance" and $c.operation == "TYPE_TEXT" and $c.target_name == "Opening balance" and $c.value_name == "opening_balance") or
    (.step_id == "submit" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "open-account-dialog" and $c.operation == "CLICK" and $c.target_name == "+ Create account") or
    (.step_id == "account-name" and $c.operation == "TYPE_TEXT" and $c.target_name == "Account name" and $c.value_name == "account_name") or
    (.step_id == "open-unit-list" and $c.operation == "CLICK" and $c.target_name == "Currency or asset") or
    (.step_id == "open-unit-dialog" and $c.operation == "CLICK" and $c.target_name == "+ New currency or asset") or
    (.step_id == "unit-symbol" and $c.operation == "TYPE_TEXT" and $c.target_name == "Symbol" and $c.value_name == "unit_symbol") or
    (.step_id == "opening-balance" and $c.operation == "TYPE_TEXT" and $c.target_name == "Opening balance" and $c.value_name == "opening_balance") or
    (.step_id == "create-account" and $c.operation == "CLICK" and $c.target_name == "Create account") or
    (.step_id == "open-account" and $c.operation == "CLICK" and $c.target_name == "Transaction wallet") or
    (.step_id == "open-transaction" and $c.operation == "CLICK" and $c.target_name == "Add transaction") or
    (.step_id == "transaction-amount" and $c.operation == "TYPE_TEXT" and $c.target_name == "Amount" and $c.value_name == "transaction_amount") or
    (.step_id == "transaction-note" and $c.operation == "TYPE_TEXT" and $c.target_name == "Note" and $c.value_name == "transaction_note") or
    (.step_id == "save-transaction" and $c.operation == "CLICK" and $c.target_name == "Save")
  ' "$payload" >/dev/null
}

# Succeeds when the result's manifest is complete and every listed Artifact matches its digest.
verify_manifest() {
  local result=$1 manifest path expected
  manifest=$(jq -r '.evidence.manifest' "$result")
  jq -e '.complete == true and ([.artifacts[].complete] | all)' "$manifest" >/dev/null || return 1
  while IFS=$'\t' read -r path expected; do
    [[ -f "$path" && "$(sha256_file "$path")" == "$expected" ]] || return 1
  done < <(jq -r '.artifacts[] | [.path,.digest] | @tsv' "$manifest")
}

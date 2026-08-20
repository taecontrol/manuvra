#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --prefix DIR --evidence-root DIR [--attempts 50] [--source-root DIR]" >&2
  exit 2
}

prefix=""
evidence_root=""
source_root=""
attempts=50
while (($#)); do
  case "$1" in
    --prefix) prefix=${2:-}; shift 2 ;;
    --evidence-root) evidence_root=${2:-}; shift 2 ;;
    --attempts) attempts=${2:-}; shift 2 ;;
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$prefix" && -n "$evidence_root" ]] || usage
[[ "$prefix" == /* && "$evidence_root" == /* ]] || { echo "paths must be absolute" >&2; exit 2; }
[[ "$attempts" =~ ^[0-9]+$ ]] && ((attempts >= 1 && attempts <= 50)) || {
  echo "--attempts must be between 1 and 50" >&2
  exit 2
}
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi
[[ ! -e "$prefix" ]] || { echo "proof prefix already exists: $prefix" >&2; exit 3; }
[[ ! -e "$evidence_root" ]] || { echo "evidence root already exists: $evidence_root" >&2; exit 3; }

mkdir -p "$evidence_root"
"$source_root/scripts/package-manuvra.sh" --prefix "$prefix" --source-root "$source_root"
cli="$prefix/bin/manuvra"

jq -n \
  --arg schema "manuvra/installed-proof-environment@1" \
  --arg commit "$(git -C "$source_root" rev-parse HEAD)" \
  --arg uname "$(uname -a)" \
  --arg macos "$(sw_vers -productVersion)" \
  --arg rustc "$(rustc --version)" \
  --arg cargo "$(cargo --version)" \
  --arg brew "$(brew --version | head -1)" \
  --argjson doctor "$("$cli" doctor)" \
  '{schema:$schema,commit:$commit,uname:$uname,macos:$macos,rustc:$rustc,cargo:$cargo,homebrew:$brew,doctor:$doctor}' \
  > "$evidence_root/environment.json"

run_set() {
  local flag=$1
  local test_name=$2
  local test_file=$3
  env \
    "$flag"=1 \
    MANUVRA_INSTALLED_CLI="$cli" \
    MANUVRA_PROOF_ROOT="$evidence_root" \
    MANUVRA_PROOF_ATTEMPTS="$attempts" \
    cargo test --locked --manifest-path "$source_root/Cargo.toml" \
      -p manuvra-cli --test "$test_file" "$test_name" -- --nocapture
}

run_set MANUVRA_RUN_INSTALLED_CHROME_PROOF installed_chrome_background_scored_set chrome_cli
run_set MANUVRA_RUN_INSTALLED_NATIVE_BACKGROUND_PROOF installed_native_background_scored_set macos_cli
run_set MANUVRA_RUN_INSTALLED_NATIVE_FOREGROUND_PROOF installed_native_foreground_scored_set macos_cli
"$cli" daemon stop >/dev/null

jq -s \
  --arg schema "manuvra/installed-proof@1" \
  --argjson attempts "$attempts" \
  '{schema:$schema,attempts:$attempts,sets:.}' \
  "$evidence_root/chrome-background/report.json" \
  "$evidence_root/native-background/report.json" \
  "$evidence_root/native-foreground/report.json" \
  > "$evidence_root/local-report.json"

inventory="$evidence_root/inventory.sha256"
find "$evidence_root" -type f ! -name 'inventory.sha256' -print0 \
  | LC_ALL=C sort -z \
  | xargs -0 shasum -a 256 > "$inventory"
echo "installed proof complete: $evidence_root"

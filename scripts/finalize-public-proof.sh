#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --source-root DIR --evidence-root DIR --crap-report FILE" >&2
  exit 2
}

source_root=""
evidence_root=""
crap_report=""
while (($#)); do
  case "$1" in
    --source-root) source_root=${2:-}; shift 2 ;;
    --evidence-root) evidence_root=${2:-}; shift 2 ;;
    --crap-report) crap_report=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$source_root" && -n "$evidence_root" && -n "$crap_report" ]] || usage
source_root=$(cd "$source_root" && pwd -P)
evidence_root=$(cd "$evidence_root" && pwd -P)
[[ -f "$crap_report" ]] || { echo "CRAP report is missing" >&2; exit 3; }
validation_target=$(mktemp -d)
trap 'rm -rf "$validation_target"' EXIT
export CARGO_TARGET_DIR="$validation_target/cargo-target"
test "$(git -C "$source_root" rev-list --count HEAD)" = 1
[[ -z $(git -C "$source_root" status --porcelain) ]] || {
  echo "public candidate must be clean before proof finalization" >&2
  exit 3
}

before=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")
"$source_root/scripts/create-public-proof-summary.sh" \
  --source-root "$source_root" \
  --evidence-root "$evidence_root" \
  --crap-report "$crap_report" \
  --output "$source_root/proof/public-proof-summary.json"
"$source_root/scripts/create-proof-certificate.sh" \
  --source-root "$source_root" \
  --crap-report "$crap_report" \
  --output "$source_root/proof/exhaustive-crap-certificate.json"
after=$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")
test "$before" = "$after"

author_date=$(git -C "$source_root" show -s --format=%aI HEAD)
committer_date=$(git -C "$source_root" show -s --format=%cI HEAD)
git -C "$source_root" add proof/public-proof-summary.json proof/exhaustive-crap-certificate.json
GIT_AUTHOR_DATE="$author_date" GIT_COMMITTER_DATE="$committer_date" \
  git -C "$source_root" commit --amend --no-edit
test "$(git -C "$source_root" rev-list --count HEAD)" = 1
test "$before" = "$("$source_root/scripts/source-tree-digest.sh" --source-root "$source_root")"
"$source_root/scripts/validate-public-snapshot.sh" "$source_root"
"$source_root/scripts/verify-proof-certificate.sh" \
  "$source_root/proof/exhaustive-crap-certificate.json" \
  --source-root "$source_root"
echo "public proof finalized in one source-bound root commit"

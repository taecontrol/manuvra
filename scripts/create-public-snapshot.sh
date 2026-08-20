#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --output EMPTY_DIR [--initialize]" >&2
  exit 2
}

output=""
initialize=false
while (($#)); do
  case "$1" in
    --output) output=${2:-}; shift 2 ;;
    --initialize) initialize=true; shift ;;
    *) usage ;;
  esac
done
[[ -n "$output" ]] || usage
repository=$(cd "$(dirname "$0")/.." && pwd -P)
if [[ -e "$output" ]] && [[ -n $(find "$output" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
  echo "snapshot output must be absent or empty" >&2
  exit 3
fi
mkdir -p "$output"
output=$(cd "$output" && pwd -P)
git -C "$repository" archive --format=tar HEAD -- . \
  ':(exclude).agents' \
  ':(exclude).claude' \
  ':(exclude).codex' \
  ':(exclude)docs/goals' \
  ':(exclude)skills-lock.json' | tar -xf - -C "$output"
"$output/scripts/validate-public-snapshot.sh" "$output"
if [[ "$initialize" == true ]]; then
  epoch=$(git -C "$repository" show -s --format=%cI HEAD)
  git -C "$output" init -b main
  git -C "$output" add --all
  GIT_AUTHOR_NAME="taecontrol" \
  GIT_AUTHOR_EMAIL="actions@users.noreply.github.com" \
  GIT_AUTHOR_DATE="$epoch" \
  GIT_COMMITTER_NAME="taecontrol" \
  GIT_COMMITTER_EMAIL="actions@users.noreply.github.com" \
  GIT_COMMITTER_DATE="$epoch" \
    git -C "$output" commit -m "Initial Manuvra v0.1.0 source"
  test "$(git -C "$output" rev-list --count HEAD)" = 1
  "$output/scripts/source-tree-digest.sh" --source-root "$output" >/dev/null
fi

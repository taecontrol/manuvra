#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 [--source-root DIR]" >&2
  exit 2
}

source_root=""
while (($#)); do
  case "$1" in
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi

manifest=$(mktemp)
trap 'rm -f "$manifest"' EXIT
while IFS= read -r -d '' path; do
  relative=${path#./}
  case "$relative" in
    .git/*|.agents/*|.claude/*|.codex/*|skills-lock.json|target/*|*/target/*|docs/goals/*|proof/exhaustive-crap-certificate.json|proof/public-proof-summary.json) continue ;;
  esac
  if [[ "$relative" == *$'\n'* || "$relative" == *$'\t'* ]]; then
    echo "source path contains an unsupported control character" >&2
    exit 3
  fi
  full="$source_root/$relative"
  mode=$(stat -f '%Lp' "$full")
  if [[ -L "$full" ]]; then
    digest=$(readlink "$full" | shasum -a 256 | awk '{print $1}')
  else
    digest=$(shasum -a 256 "$full" | awk '{print $1}')
  fi
  printf '%s\t%s\t%s\n' "$relative" "$mode" "$digest" >> "$manifest"
done < <(git -C "$source_root" ls-files -z | LC_ALL=C sort -z)
[[ -s "$manifest" ]] || { echo "source tree has no release inputs" >&2; exit 3; }
shasum -a 256 "$manifest" | awk '{print $1}'

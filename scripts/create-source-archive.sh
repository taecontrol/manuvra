#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --version X.Y.Z --output DIR" >&2
  exit 2
}

version=""
output=""
while (($#)); do
  case "$1" in
    --version)
      version=${2:-}
      shift 2
      ;;
    --output)
      output=${2:-}
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && -n "$output" ]] || usage

repository=$(cd "$(dirname "$0")/.." && pwd -P)
head_version=$(git -C "$repository" show HEAD:Cargo.toml | awk -F '"' '/^version = / { print $2; exit }')
[[ "$head_version" == "$version" ]] || {
  echo "requested version differs from the Cargo workspace version at HEAD" >&2
  exit 3
}

mkdir -p "$output"
output=$(cd "$output" && pwd -P)
archive="$output/manuvra-$version.tar.gz"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

git -C "$repository" archive \
  --format=tar.gz \
  --prefix="manuvra-$version/" \
  --output="$temporary/archive.tar.gz" \
  HEAD
mv "$temporary/archive.tar.gz" "$archive"

digest=$(shasum -a 256 "$archive" | awk '{print $1}')
printf '%s  %s\n' "$digest" "$(basename "$archive")" > "$archive.sha256"

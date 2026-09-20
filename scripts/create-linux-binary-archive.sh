#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --version X.Y.Z --arch x64|arm64 --output DIR" >&2
  exit 2
}

version=""
arch=""
output=""
while (($#)); do
  case "$1" in
    --version)
      version=${2:-}
      shift 2
      ;;
    --arch)
      arch=${2:-}
      shift 2
      ;;
    --output)
      output=${2:-}
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$arch" =~ ^(x64|arm64)$ && -n "$output" ]] || usage

repository=$(cd "$(dirname "$0")/.." && pwd -P)
head_version=$(git -C "$repository" show HEAD:Cargo.toml | awk -F '"' '/^version = / { print $2; exit }')
[[ "$head_version" == "$version" ]] || {
  echo "requested version differs from the Cargo workspace version at HEAD" >&2
  exit 3
}

case "$(uname -m)" in
  x86_64) runner_arch=x64 ;;
  aarch64 | arm64) runner_arch=arm64 ;;
  *)
    echo "unsupported build architecture: $(uname -m)" >&2
    exit 4
    ;;
esac
[[ "$runner_arch" == "$arch" ]] || {
  echo "requested architecture $arch does not match runner architecture $runner_arch" >&2
  exit 4
}
[[ "$(uname -s)" == Linux ]] || {
  echo "Linux binary archives must be built on Linux" >&2
  exit 4
}

source_date_epoch=$(git -C "$repository" show -s --format=%ct HEAD)
SOURCE_DATE_EPOCH="$source_date_epoch" cargo build \
  --manifest-path "$repository/Cargo.toml" \
  --release \
  --locked \
  --package manuvra-cli \
  --bin manuvra

binary="$repository/target/release/manuvra"
[[ -x "$binary" ]] || {
  echo "release build did not produce an executable manuvra binary" >&2
  exit 5
}
actual_version=$("$binary" version | python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])')
[[ "$actual_version" == "$version" ]] || {
  echo "built binary version differs from the requested version" >&2
  exit 5
}

mkdir -p "$output"
output=$(cd "$output" && pwd -P)
archive="$output/manuvra-$version-linux-$arch.tar.gz"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
install -m 0755 "$binary" "$temporary/manuvra"

tar \
  --sort=name \
  --mtime="@$source_date_epoch" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  --format=ustar \
  -C "$temporary" \
  -czf "$archive" \
  manuvra

digest=$(sha256sum "$archive" | awk '{print $1}')
printf '%s  %s\n' "$digest" "$(basename "$archive")" > "$archive.sha256"

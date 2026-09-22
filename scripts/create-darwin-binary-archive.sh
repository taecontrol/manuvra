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
      (($# >= 2)) || usage
      version=${2:-}
      shift 2
      ;;
    --arch)
      (($# >= 2)) || usage
      arch=${2:-}
      shift 2
      ;;
    --output)
      (($# >= 2)) || usage
      output=${2:-}
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$arch" =~ ^(x64|arm64)$ && -n "$output" ]] || usage

[[ "$(uname -s)" == Darwin ]] || {
  echo "Darwin binary archives must be built on Darwin" >&2
  exit 4
}

case "$(uname -m)" in
  x86_64) runner_arch=x64 ;;
  arm64) runner_arch=arm64 ;;
  *)
    echo "unsupported build architecture: $(uname -m)" >&2
    exit 4
    ;;
esac
[[ "$runner_arch" == "$arch" ]] || {
  echo "requested architecture $arch does not match runner architecture $runner_arch" >&2
  exit 4
}

repository=$(cd "$(dirname "$0")/.." && pwd -P)
head_version=$(git -C "$repository" show HEAD:Cargo.toml | awk -F '"' '/^version = / { print $2; exit }')
[[ "$head_version" == "$version" ]] || {
  echo "requested version differs from the Cargo workspace version at HEAD" >&2
  exit 3
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

case "$arch" in
  x64) macho_arch=x86_64 ;;
  arm64) macho_arch=arm64 ;;
esac
actual_macho_arch=$(lipo -archs "$binary")
[[ "$actual_macho_arch" == "$macho_arch" ]] || {
  echo "built binary architecture $actual_macho_arch differs from requested Mach-O architecture $macho_arch" >&2
  exit 5
}

mkdir -p "$output"
output=$(cd "$output" && pwd -P)
archive="$output/manuvra-$version-darwin-$arch.tar.gz"
checksum="$archive.sha256"
temporary=$(mktemp -d "$output/.manuvra-darwin-archive.XXXXXX")
publishing=0
cleanup() {
  status=$?
  trap - EXIT
  if [[ "$status" -ne 0 && "$publishing" -eq 1 ]]; then
    rm -f "$archive" "$checksum"
  fi
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT
mkdir "$temporary/payload"
install -m 0755 "$binary" "$temporary/payload/manuvra"
xattr -c "$temporary/payload/manuvra"

archive_timestamp=$(date -r "$source_date_epoch" '+%Y%m%d%H%M.%S')
touch -t "$archive_timestamp" "$temporary/payload/manuvra"
COPYFILE_DISABLE=1 tar \
  -cf "$temporary/archive.tar" \
  --format ustar \
  --uid 0 \
  --gid 0 \
  --uname root \
  --gname wheel \
  -C "$temporary/payload" \
  manuvra
gzip -9 -n -c "$temporary/archive.tar" > "$temporary/$(basename "$archive")"

digest=$(shasum -a 256 "$temporary/$(basename "$archive")" | awk '{print $1}')
printf '%s  %s\n' "$digest" "$(basename "$archive")" > "$temporary/$(basename "$checksum")"

publishing=1
mv "$temporary/$(basename "$archive")" "$archive"
mv "$temporary/$(basename "$checksum")" "$checksum"
publishing=0

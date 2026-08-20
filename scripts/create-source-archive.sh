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
    --version) version=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && -n "$output" ]] || usage
repository=$(cd "$(dirname "$0")/.." && pwd -P)
[[ $(awk -F '"' '/^version = / { print $2; exit }' "$repository/Cargo.toml") == "$version" ]] || {
  echo "requested version differs from Cargo workspace version" >&2
  exit 3
}
source_tree_sha256=$("$repository/scripts/source-tree-digest.sh" --source-root "$repository")
[[ -z $(git -C "$repository" status --porcelain --untracked-files=no) ]] || {
  echo "tracked source must be clean before creating a release archive" >&2
  exit 3
}

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
root="manuvra-$version"
mkdir -p "$temporary/$root" "$output"
git -C "$repository" archive HEAD | tar -xf - -C "$temporary/$root"
"$temporary/$root/scripts/generate-release-manifest.sh" \
  --source-root "$temporary/$root" \
  --output "$temporary/$root/release-manifest.json" \
  --source-tree-sha256 "$source_tree_sha256"
epoch=$(git -C "$repository" show -s --format=%ct HEAD)
find "$temporary/$root" -exec touch -h -t "$(date -u -r "$epoch" +%Y%m%d%H%M.%S)" {} +
list="$temporary/files.txt"
(cd "$temporary" && find "$root" -print | LC_ALL=C sort > "$list")
archive="$output/manuvra-$version.tar.gz"
COPYFILE_DISABLE=1 tar --no-recursion --format ustar --uid 0 --gid 0 --numeric-owner -C "$temporary" -cf - -T "$list" | gzip -n > "$archive"
duplicates=$(tar -tzf "$archive" | LC_ALL=C sort | uniq -d)
[[ -z "$duplicates" ]] || {
  echo "source archive contains duplicate entries:" >&2
  printf '%s\n' "$duplicates" >&2
  exit 3
}
digest=$(shasum -a 256 "$archive" | awk '{print $1}')
printf '%s  %s\n' "$digest" "$(basename "$archive")" > "$archive.sha256"

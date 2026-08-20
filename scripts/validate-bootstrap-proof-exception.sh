#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 --version VERSION [--source-root DIR]" >&2
  exit 2
}

version=""
source_root=""
while (($#)); do
  case "$1" in
    --version) version=${2:-}; shift 2 ;;
    --source-root) source_root=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[[ -n "$version" ]] || usage
if [[ -z "$source_root" ]]; then
  source_root=$(cd "$(dirname "$0")/.." && pwd -P)
else
  source_root=$(cd "$source_root" && pwd -P)
fi

[[ "$version" == "0.2.0" ]] || {
  echo "the bootstrap proof exception is valid only for v0.2.0" >&2
  exit 3
}
[[ ! -e "$source_root/proof/exhaustive-crap-certificate.json" ]] || {
  echo "a bootstrap release must not ship a stale exhaustive certificate" >&2
  exit 3
}
[[ ! -e "$source_root/proof/public-proof-summary.json" ]] || {
  echo "a bootstrap release must not ship a stale public proof summary" >&2
  exit 3
}

exception="$source_root/proof/v0.2.0-bootstrap-exception.json"
jq -e --arg version "$version" '
  .schema == "manuvra/bootstrap-proof-exception@1" and
  .version == $version and
  .status == "authorized" and
  .omitted_artifacts == [
    "proof/exhaustive-crap-certificate.json",
    "proof/public-proof-summary.json"
  ] and
  .scope.tag == ("v" + $version) and
  .scope.reusable == false
' "$exception" >/dev/null
echo "documented one-time bootstrap proof exception accepted for v$version"

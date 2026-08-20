#!/bin/bash
set -euo pipefail

root=${1:-.}
root=$(cd "$root" && pwd -P)
for excluded in .agents .claude .codex docs/goals skills-lock.json target; do
  [[ ! -e "$root/$excluded" ]] || {
    echo "public snapshot contains excluded path: $excluded" >&2
    exit 3
  }
done

if rg -n --hidden \
  -g '!.git/**' \
  -e '/Users/[A-Za-z0-9._-]+/' \
  -e 'BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY' \
  -e 'gh[pousr]_[A-Za-z0-9_]{20,}' \
  -e 'github_pat_[A-Za-z0-9_]{20,}' \
  "$root"; then
  echo "public snapshot contains a personal path or credential-shaped value" >&2
  exit 3
fi

legacy=$(cd "$root" && rg -n --hidden -g '!.git/**' -g '!scripts/validate-public-snapshot.sh' 'computer-use|computer_use|COMPUTER_USE|Computer Use|Computer-use' . || true)
unexpected=$(printf '%s\n' "$legacy" | rg -v \
  'migrate --from computer-use|"migrate", "--from", "computer-use"|\.config/computer-use|LegacySource|ComputerUse|legacy_source|MANUVRA_LEGACY_CONFIG_HOME|legacy configuration|No legacy `computer-use`|_Avoid_: Computer Use, computer-use|"from": "computer-use"|const": "computer-use"|value\(name = "computer-use"\)|=> "computer-use"' || true)
if [[ -n "$unexpected" ]]; then
  printf '%s\n' "$unexpected" >&2
  echo "public snapshot contains an unexpected legacy product namespace" >&2
  exit 3
fi

gitleaks dir "$root" --no-banner --redact

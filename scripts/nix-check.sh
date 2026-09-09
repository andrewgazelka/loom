#!/usr/bin/env bash
set -u
cd "$(dirname "$0")/.."
passed=0
first=''
check() {
  label=$1
  shift
  if "$@"; then passed=$((passed + 1));
  elif [[ -z "$first" ]]; then first=$label; fi
}
check 'Nix package' nix build .#default --no-link
check 'Nix launcher' nix run . -- --help
check 'Nix guest execution' bash scripts/nix-e2e.sh
printf '%s/3 Nix gates pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 3 ]]

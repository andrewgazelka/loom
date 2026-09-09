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
check 'frontend delivery' bash scripts/milestones/m10.sh
check 'CAS browser API' cargo test --locked -p loom-api --test cas_browser
if [[ -n ${LOOM_URL:-} ]]; then
  check 'live CAS navigation' bun scripts/site-e2e.ts
else
  check 'live CAS navigation' env LOOM_E2E_SUITE=site bash scripts/native-e2e.sh
fi
printf '%s/3 website gates pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 3 ]]

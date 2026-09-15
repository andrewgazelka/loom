#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# A missing suite is a failing integration, never a successful zero-test run.
for required in crates/loom-v8/Cargo.toml crates/loom-behavior/tests/sandbox.rs crates/loom-api/tests/javascript.rs crates/loom-rt/tests/sandbox.rs; do
  if [[ ! -f "$required" ]]; then
    printf '0/5 sandbox suites pass; first failing step: missing %s\n' "$required"
    exit 1
  fi
done

passed=0
first=''
run_suite() {
  local name="$1"
  shift
  if "$@"; then
    passed=$((passed + 1))
  elif [[ -z "$first" ]]; then
    first="$name"
  fi
}
run_suite v8 cargo test --locked -p loom-v8
run_suite actors cargo test --locked -p loom-behavior --test sandbox
run_suite admission cargo test --locked -p loom-api --test javascript
run_suite runtime cargo test --locked -p loom-rt --test sandbox
run_suite persistence cargo test --locked -p loom-store --test language --test javascript
printf '%s/5 sandbox suites pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 5 ]]

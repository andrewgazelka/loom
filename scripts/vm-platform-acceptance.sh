#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"
export CARGO_TERM_COLOR=never
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
run_test() {
  local output status
  output=$(mktemp)
  # A cfg-disabled target exits successfully with zero tests. Require a real
  # passing test summary as well as Cargo success, including for filtered libs.
  if cargo test --locked "$@" 2>&1 | tee "$output"; then
    status=0
    if ! rg -q 'test result: ok\. [1-9][0-9]* passed;' "$output"; then
      printf 'No executed test witness for %s\n' "$*" >&2
      status=1
    fi
  else
    status=1
  fi
  rm -f "$output"
  return "$status"
}
run_cas() {
  run_test -p loom-proto --test dag_cbor || return
  run_test -p loom-store --test cas_files || return
  run_test -p loom-api --test cas_actors || return
  run_test -p loom-api --test cas_upload || return
  run_test -p loom-api --test vm_images
}
run_vm() {
  run_test -p loom-proto --lib vm::tests || return
  run_test -p loom-vm-runner --bin loom-vm-runner || return
  run_test -p loom-actor --test vm_driver
}
run_daemon() {
  if [[ ! -f tools/smoke-loomd-vm.py || -z "${LOOM_TEST_DAEMON:-}" ]]; then
    printf 'Missing VM daemon POC or LOOM_TEST_DAEMON\n' >&2
    return 1
  fi
  python3 tools/smoke-loomd-vm.py "$LOOM_TEST_DAEMON"
}
if [[ $(uname -s) != Linux ]]; then
  printf '0/4 VM platform suites pass; first failing step: native Linux required\n'
  exit 1
fi
run_suite javascript run_test -p loom-v8 --test resources
run_suite cas run_cas
run_suite linux_vm run_vm
run_suite daemon run_daemon
printf '%s/4 VM platform suites pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 4 ]]

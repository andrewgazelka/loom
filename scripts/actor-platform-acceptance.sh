#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Each fixture owns native io_uring rings. Keep the default test fan-out within
# ordinary user-service memlock limits; individual concurrency controls still run.
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"
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
  local package="$1" target="$2"
  if [[ ! -f "crates/$package/tests/$target.rs" ]]; then
    printf 'Missing %s/tests/%s.rs\n' "$package" "$target" >&2
    return 1
  fi
  cargo test --locked -p "$package" --test "$target"
}
run_daemon() {
  if [[ -z "${LOOM_TEST_DAEMON:-}" || -z "${LOOM_TEST_DOCKER:-}" || -z "${LOOM_TEST_DOCKER_HOST:-}" || -z "${LOOM_TEST_CONTAINER_IMAGE:-}" || -z "${LOOM_TEST_CLAUDE_IMAGE:-}" ]]; then
    printf 'Daemon POC requires LOOM_TEST_DAEMON, LOOM_TEST_DOCKER, LOOM_TEST_DOCKER_HOST, LOOM_TEST_CONTAINER_IMAGE and LOOM_TEST_CLAUDE_IMAGE\n' >&2
    return 1
  fi
  python3 tools/smoke-loomd-v8.py "$LOOM_TEST_DAEMON" --root "$PWD" --npm \
    --docker-executable "$LOOM_TEST_DOCKER" --docker-host "$LOOM_TEST_DOCKER_HOST" \
    --docker-image "$LOOM_TEST_CONTAINER_IMAGE" --claude-image "$LOOM_TEST_CLAUDE_IMAGE"
}
run_suite sandbox bash scripts/sandbox-acceptance.sh
run_suite websockets run_test loom-api actor_websocket
run_suite processes run_test loom-actor process_driver
run_suite tenants run_test loom-api tenant_actors
run_suite typescript run_test loom-api typescript
run_suite imports run_test loom-api imports
run_suite containers run_test loom-actor container_driver
run_suite lifecycle run_test loom-actor lifecycle_hooks
run_suite daemon run_daemon
printf '%s/9 actor platform suites pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 9 ]]

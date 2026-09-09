#!/usr/bin/env bash
set -u
cd "$(dirname "$0")/.."
passed=0
first=''
server_pid=''
scratch=''
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill -INT "$server_pid" || true
    wait "$server_pid" || true
  fi
  [[ -z "$scratch" ]] || rm -rf "$scratch"
}
trap cleanup EXIT INT TERM
# A single command exercises the service it builds. An explicitly supplied URL
# and token instead tests an already running daemon.
if [[ -z "${LOOM_URL:-}" ]]; then
  scratch=$(mktemp -d)
  if (cd loom-checker && bun install --frozen-lockfile) &&
     (cd loom-guest-ts && bun install --frozen-lockfile) &&
     cargo build --locked --release -p loomd; then
    cp "${CARGO_TARGET_DIR:-target}/release/loomd" "$scratch/loomd"
    export LOOMD_BINARY="$scratch/loomd"
    export LOOM_TOKEN=loom-acceptance-test LOOM_URL=http://127.0.0.1:18787
    "$LOOMD_BINARY" --root "$PWD" --db "$scratch/loom.sqlite" --bind 127.0.0.1:18787 >"$scratch/daemon.log" 2>&1 &
    server_pid=$!
    ready=false
    for attempt in $(seq 1 100); do
      if bun -e 'fetch(process.env.LOOM_URL+"/health").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))'; then ready=true; break; fi
      kill -0 "$server_pid" || break
      sleep .1
    done
    if [[ "$ready" != true ]]; then cat "$scratch/daemon.log"; fi
  else
    echo 'Service bootstrap failed; dependent milestones will fail.' >&2
  fi
fi
for n in $(seq 1 11); do
  test_path="scripts/milestones/m${n}.sh"
  if [[ -x "$test_path" ]] && "$test_path"; then
    passed=$((passed + 1))
  else
    [[ -n "$first" ]] || first="M${n}: ${test_path} missing or failing"
  fi
done
printf '%s/11 milestones pass; first failing step: %s\n' "$passed" "${first:-none}"
[[ "$passed" -eq 11 ]]

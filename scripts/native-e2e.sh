#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
(cd loom-checker && bun install --frozen-lockfile)
(cd loom-guest-ts && bun install --frozen-lockfile)
cargo build --locked --release -p loomd
scratch=$(mktemp -d)
server_pid=''
cleanup() {
  if [[ -n "$server_pid" ]]; then kill -INT "$server_pid" || true; wait "$server_pid" || true; fi
  rm -rf "$scratch"
}
trap cleanup EXIT INT TERM
cp "${CARGO_TARGET_DIR:-target}/release/loomd" "$scratch/loomd"
export LOOM_TOKEN=loom-native-test LOOM_URL=http://127.0.0.1:18787
"$scratch/loomd" --root "$root" --db "$scratch/loom.sqlite" --bind 127.0.0.1:18787 >"$scratch/daemon.log" 2>&1 &
server_pid=$!
ready=false
for attempt in $(seq 1 100); do
  if bun -e 'fetch(process.env.LOOM_URL+"/health").then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))'; then ready=true; break; fi
  if ! kill -0 "$server_pid"; then cat "$scratch/daemon.log"; exit 1; fi
  sleep .1
done
if [[ "$ready" != true ]]; then cat "$scratch/daemon.log"; exit 1; fi
case "${LOOM_E2E_SUITE:-all}" in
  languages) if ! bun scripts/languages-e2e.ts; then cat "$scratch/daemon.log"; exit 1; fi ;;
  mcp) if ! bun scripts/mcp-e2e.ts; then cat "$scratch/daemon.log"; exit 1; fi ;;
  all)
    if ! bun scripts/e2e.ts; then cat "$scratch/daemon.log"; exit 1; fi
    if ! bun scripts/mcp-e2e.ts; then cat "$scratch/daemon.log"; exit 1; fi ;;
  *) echo 'Unknown LOOM_E2E_SUITE' >&2; exit 64 ;;
esac

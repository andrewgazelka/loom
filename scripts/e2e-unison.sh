#!/usr/bin/env bash
# Requires nix, bun, and curl. The launcher supplies the matching CLI.
set -euo pipefail
cd "$(dirname "$0")/.."
passed=0
finished=false
server_pid=
state=
finish() {
  local status=$?
  trap - EXIT
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if ! "$finished"; then
    for ((n=passed+1; n<=9; n++)); do
      printf 'FAIL %s setup: proof interrupted; see diagnostics above\n' "$n"
    done
    printf '%s/9\n' "$passed"
  fi
  [[ -z "$state" ]] || printf 'State and logs: %s\n' "$state" >&2
  exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
for tool in nix bun curl; do command -v "$tool" >/dev/null; done
LOOM_E2E_PORT=${LOOM_E2E_PORT:-8787}
# Refuse an occupied port, including an unrelated HTTP server.
if (exec 3<>"/dev/tcp/127.0.0.1/$LOOM_E2E_PORT") 2>/dev/null; then
  printf 'Port %s is occupied; choose another LOOM_E2E_PORT.\n' "$LOOM_E2E_PORT" >&2
  exit 1
fi
state=$(mktemp -d "${TMPDIR:-/tmp}/loom-unison.XXXXXXXX")
export LOOM_DATA_DIR="$state/data"
export LOOM_BUILD_DIR="$state/builds"
export LOOM_URL="http://127.0.0.1:$LOOM_E2E_PORT"
unset LOOM_TOKEN LOOM_BIND
nix run --builders '' .#repl -- --bind "127.0.0.1:$LOOM_E2E_PORT" >"$state/launcher.log" 2>&1 &
server_pid=$!
# Allow the initial local Nix build to finish. Keep its log available on failure.
deadline=$((SECONDS + ${LOOM_E2E_START_TIMEOUT:-3600}))
ready=false
while ((SECONDS < deadline)); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$state/launcher.log" >&2
    exit 1
  fi
  token_file=$(sed -n 's/^Token file: //p' "$state/launcher.log" | tail -n 1)
  cli_path=$(sed -n 's/^CLI: //p' "$state/launcher.log" | tail -n 1)
  if [[ -n "$token_file" && -s "$token_file" && -x "$cli_path" ]]; then
    LOOM_TOKEN=$(cat "$token_file")
    export LOOM_TOKEN
    if curl --silent --fail --max-time 2 "$LOOM_URL/" >/dev/null; then
      export PATH="$(dirname "$cli_path"):$PATH"
      ready=true
      break
    fi
  fi
  sleep 1
done
if ! "$ready"; then cat "$state/launcher.log" >&2; exit 1; fi
# Copy fixtures so the source-removal check never moves tracked user files.
mkdir "$state/cli" "$state/mcp"
cp examples/unison/*.rs "$state/cli/"
cp examples/unison/*.rs "$state/mcp/"
cli_status=0
bun scripts/e2e-unison-mcp.ts --cli "$state/cli" | tee "$state/cli.log" || cli_status=$?
passed=$(awk '/^ok [1-8] / { n++ } END { print n+0 }' "$state/cli.log")
mcp_status=0
bun scripts/e2e-unison-mcp.ts --mcp "$state/mcp" | tee "$state/mcp.log" || mcp_status=$?
if [[ "$mcp_status" == 0 ]] && [[ $(tail -n 1 "$state/mcp.log") == 9/9 ]]; then
  printf 'ok 9 mcp\n'
  passed=$((passed + 1))
else
  printf 'FAIL 9 mcp: companion did not complete 9/9\n'
fi
finished=true
printf '%s/9\n' "$passed"
[[ "$passed" == 9 && "$cli_status" == 0 && "$mcp_status" == 0 ]]

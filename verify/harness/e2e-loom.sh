#!/usr/bin/env bash
# Run the verified harness actor on a live Loom daemon and replay the races the
# checker found in step_v1. Usage: e2e-loom.sh <launcher.log>
set -euo pipefail
log=$1
here="$(cd "$(dirname "$0")" && pwd)"
url=${LOOM_URL:?set LOOM_URL, e.g. http://127.0.0.1:8811}
export LOOM_TOKEN="$(cat "$(sed -n 's/^Token file: //p' "$log" | tail -1)")"
loom="$(sed -n 's/^CLI: //p' "$log" | tail -1)"
echo "daemon $url, cli $loom"

"$loom" --url "$url" add "$here/harness.rs" --lang rust --name harness
id=$("$loom" --url "$url" spawn harness | tee /dev/stderr | grep -oE '[0-9a-f-]{8,}' | head -1)
echo "actor $id"

send() { "$loom" --url "$url" send "$id" "$1" >/dev/null; }
effects() { "$loom" --url "$url" sql "$id" "SELECT kind, id, outcome FROM effects ORDER BY seq"; }
count() { "$loom" --url "$url" sql "$id" "SELECT count(*) AS n FROM effects" | grep -oE '[0-9]+' | tail -1; }
wait_for() { for _ in $(seq 60); do [ "$(count)" = "$1" ] && return 0; sleep 1; done; echo "timeout waiting for $1 effects (have $(count))"; return 1; }

# The races from Check.lean, on the fixed code:
send '{"tool_use":1}';            wait_for 1   # ask 1
send '{"permission":[1,true]}';   wait_for 2   # run 1
send '{"cancel":null}';           wait_for 4   # abort 1, result 1 cancelled
send '{"tool_done":1}'                         # late tool exit: must emit nothing
send '{"cancel":null}'                         # double cancel: must emit nothing
send '{"permission":[1,true]}'                 # late approval: must not run
send '{"permision":[1,true]}'                  # typo: must dead-letter, not cancel
send '{"tool_use":2}';            wait_for 5   # after cancel: result 2 cancelled, no prompt
effects
"$loom" --url "$url" info "$id"
echo "E2E-HARNESS-DONE"

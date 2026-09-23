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

name="harness-$(date +%s)"
"$loom" --url "$url" add "${HARNESS:-$here/harness.rs}" --lang rust --name "$name"
id=$("$loom" --url "$url" spawn "$name" | tee /dev/stderr | grep -oE '[0-9a-f-]{8,}' | head -1)
echo "actor $id"

send() { "$loom" --url "$url" send "$id" "$1" >/dev/null; }
effects() { "$loom" --url "$url" sql "$id" "SELECT kind, id, outcome FROM harness_effects ORDER BY seq"; }
count() { "$loom" --url "$url" sql "$id" "SELECT count(*) AS n FROM harness_effects" | grep -oE '[0-9]+' | tail -1; }
wait_for() { for _ in $(seq 60); do [ "$(count)" -ge "$1" ] && return 0; sleep 1; done; echo "timeout waiting for $1 effects (have $(count))"; return 1; }

# The races from Check.lean, on the fixed code:
send '{"tool_use":1}';            wait_for 1   # ask 1
send '{"permission":[1,true]}';   wait_for 2   # run 1
send '{"cancel":null}';           wait_for 4   # abort 1, result 1 cancelled
send '{"tool_done":1}'                         # late tool exit: must emit nothing
send '{"cancel":null}'                         # double cancel: must emit nothing
send '{"permission":[1,true]}'                 # late approval: must not run
send '{"permision":[1,true]}'                  # typo: recorded as rejected, changes nothing
send '{"tool_use":2}';            wait_for 5   # after cancel: result 2 cancelled, no prompt

got=$(effects | python3 -c 'import json,sys; print(json.dumps([[r["kind"], r["id"], r["outcome"]] for r in json.load(sys.stdin)["result"]]))')
want='[["ask_permission", 1, null], ["run_tool", 1, null], ["abort_tool", 1, null], ["result", 1, "cancelled"], ["result", 2, "cancelled"]]'
rejected=$("$loom" --url "$url" sql "$id" "SELECT reason FROM harness_rejected" | python3 -c 'import json,sys; print(json.dumps([r["reason"] for r in json.load(sys.stdin)["result"]]))')
status=$("$loom" --url "$url" info "$id" | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"].get("status"))')
echo "effects:  $got"
echo "rejected: $rejected"
echo "status:   $status"
fail=0
[ "$got" = "$want" ] || { echo "FAIL effects, want $want"; fail=1; }
[ "$rejected" = '["unknown message \"permision\""]' ] || { echo "FAIL rejected"; fail=1; }
[ "$status" = "running" ] || { echo "FAIL status"; fail=1; }
[ "$fail" = 0 ] && echo "E2E-HARNESS-PASS" || { echo "E2E-HARNESS-FAIL"; exit 1; }

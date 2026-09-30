#!/usr/bin/env bash
# Goal command for the REPL: how long from "here is a Rust cell" to "here is its
# output". Sends N different cells to `eval`, each one a fresh edit (a new
# string constant and a new arithmetic body), and prints the median and p90 wall
# time of the whole request plus the server's own stage timings.
#
#   LOOM_URL=http://127.0.0.1:8787 LOOM_TOKEN=... scripts/bench/repl-latency.sh [N] [LIMIT_MS]
#
# The first eval of a session builds the dependency graph (cold, tens of
# seconds on a fresh daemon) and is reported but excluded from the statistics.
# Every measured cell's output is checked against the value computed here, so a
# fast wrong answer fails. Exit status is non-zero when any eval fails or is
# wrong, or when the median exceeds LIMIT_MS (default 100). Requires bash and
# python3.
set -euo pipefail
: "${LOOM_URL:?LOOM_URL is required}"
: "${LOOM_TOKEN:?LOOM_TOKEN is required}"
count=${1:-20}
limit=${2:-100}
case "$count" in ''|*[!0-9]*|0) echo "repl-latency: N must be a positive integer, got '$count'" >&2; exit 64;; esac
case "$limit" in ''|*[!0-9]*) echo "repl-latency: LIMIT_MS must be an integer, got '$limit'" >&2; exit 64;; esac
health=$(curl -sS -o /dev/null -w '%{http_code}' "${LOOM_URL%/}/health") || { echo "repl-latency: daemon at $LOOM_URL is unreachable" >&2; exit 69; }
[ "$health" = 200 ] || { echo "repl-latency: daemon health returned HTTP $health" >&2; exit 69; }
export LOOM_URL LOOM_TOKEN
exec python3 - "$count" "$limit" <<'PY'
import json, os, statistics, sys, time, urllib.error, urllib.request

count, limit = int(sys.argv[1]), int(sys.argv[2])
url = os.environ["LOOM_URL"].rstrip("/")
token = os.environ["LOOM_TOKEN"]
nonce = f"{int(time.time() * 1000):x}"


def request(body):
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        f"{url}/v1/command", data=data, method="POST",
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=1800) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"{}")


def cell(index):
    """A cell with a fresh constant and body, and the output it must produce."""
    source = (
        f'pub fn run(x: u64) -> String {{\n'
        f'    let scaled = x * {index + 3} + {index};\n'
        f'    format!("cell-{nonce}-{index}:{{scaled}}")\n'
        f'}}\n'
    )
    return source, f"cell-{nonce}-{index}:{7 * (index + 3) + index}"


walls, computes, failures = [], [], 0
for index in range(count + 1):
    source, expected = cell(index)
    started = time.perf_counter()
    status, reply = request({"command": "eval", "args": {"source": source, "args": [7]}})
    wall_ms = (time.perf_counter() - started) * 1000
    if status != 200 or not reply.get("ok"):
        failures += 1
        print(f"eval i={index} FAILED http={status} reply={json.dumps(reply)[:1500]}")
        continue
    result = reply["result"]
    if result.get("output") != expected:
        failures += 1
        print(f"eval i={index} WRONG output={result.get('output')!r} expected={expected!r}")
        continue
    timings = result.get("timings_ms", {})
    label = "cold" if index == 0 else "warm"
    print(f"eval i={index} {label} wall_ms={wall_ms:.0f} compile_ms={timings.get('compile')} run_ms={timings.get('run')}")
    if index > 0:
        walls.append(wall_ms)
        computes.append(timings.get("compile", 0) + timings.get("run", 0))

if not walls:
    print("repl-latency FAILED no measured eval succeeded")
    sys.exit(1)
walls.sort()
p90 = walls[min(len(walls) - 1, int(len(walls) * 0.9))]
median = statistics.median(walls)
verdict = "PASS" if failures == 0 and median <= limit else "FAIL"
print(f"repl-latency median_wall_ms={median:.0f} p90_wall_ms={p90:.0f} server_median_ms={statistics.median(computes):.0f} n={len(walls)} limit_ms={limit} failures={failures} {verdict}")
sys.exit(0 if verdict == "PASS" else 1)
PY

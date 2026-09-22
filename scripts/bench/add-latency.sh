#!/usr/bin/env bash
# Goal command for warm `add` latency. Adds N fresh single-file Rust definitions
# to a running daemon, each differing by one string constant, and prints the
# median wall time, the median of the API's `build.ms`, and one median per
# build stage read from the build log (`build_stages` JSON lines).
#
#   LOOM_URL=http://127.0.0.1:8787 LOOM_TOKEN=... scripts/bench/add-latency.sh [N]
#
# Every add reuses one definition name (LOOM_BENCH_NAME, default add-latency)
# so the per-lineage incremental directory is warm from the second add on; the
# first add of a fresh daemon also pays the cold Cargo resolve of its graph and
# is reported per add, not excluded from the median. LOOM_BENCH_SOURCE names a
# Rust file whose first string literal is replaced per add; the default is
# examples/unison/greet.rs. Requires bash, curl and python3. Exit status is
# non-zero when any add fails, any log cannot be fetched, or a build log lacks a
# `build_stages` line.
set -euo pipefail
: "${LOOM_URL:?LOOM_URL is required}"
: "${LOOM_TOKEN:?LOOM_TOKEN is required}"
count=${1:-5}
here=$(cd "$(dirname "$0")" && pwd)
source_file=${LOOM_BENCH_SOURCE:-$here/../../examples/unison/greet.rs}
name=${LOOM_BENCH_NAME:-add-latency}
[ -r "$source_file" ] || { echo "add-latency: source file $source_file is unreadable" >&2; exit 64; }
case "$count" in ''|*[!0-9]*|0) echo "add-latency: N must be a positive integer, got '$count'" >&2; exit 64;; esac
health=$(curl -sS -o /dev/null -w '%{http_code}' "${LOOM_URL%/}/health") || { echo "add-latency: daemon at $LOOM_URL is unreachable" >&2; exit 69; }
[ "$health" = 200 ] || { echo "add-latency: daemon health returned HTTP $health" >&2; exit 69; }
export LOOM_URL LOOM_TOKEN
exec python3 - "$count" "$source_file" "$name" <<'PY'
import json, os, re, statistics, sys, time, urllib.error, urllib.request

count, source_file, name = int(sys.argv[1]), sys.argv[2], sys.argv[3]
url = os.environ["LOOM_URL"].rstrip("/")
token = os.environ["LOOM_TOKEN"]
# Stage attribution must cover the build within this share of build.ms; the
# builder's own limit (crates/loom-build/src/stages.rs) is the same number.
UNATTRIBUTED_LIMIT_PERCENT = 5

template = open(source_file, encoding="utf-8").read()
literal = re.search(r'"([^"\\]*)"', template)
if literal is None:
    sys.exit(f"add-latency: {source_file} has no string literal to vary")
nonce = f"{int(time.time() * 1000):x}"


def request(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Authorization": f"Bearer {token}"}
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(f"{url}{path}", data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=1800) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def stage_lines(log):
    """Every JSON object line carrying `build_stages`, merged in order."""
    stages, seen = {}, False
    for line in log.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and isinstance(value.get("build_stages"), dict):
            seen = True
            stages.update(value["build_stages"])
    return stages if seen else None


walls, builds, per_stage, failures = [], [], {}, 0
worst_unattributed = 0.0
for index in range(1, count + 1):
    source = template.replace(f'"{literal.group(1)}"', f'"{literal.group(1)}-{nonce}-{index}"', 1)
    body = {"command": "add", "args": {"name": name, "lang": "rust", "source": source}}
    started = time.perf_counter()
    status, raw = request("POST", "/v1/command", body)
    wall_ms = (time.perf_counter() - started) * 1000
    try:
        reply = json.loads(raw)
    except json.JSONDecodeError:
        reply = {"ok": False, "result": raw.decode(errors="replace")}
    if status != 200 or not reply.get("ok"):
        failures += 1
        print(f"add i={index} FAILED http={status} reply={json.dumps(reply)[:2000]}")
        continue
    build = reply["result"]["build"]
    build_ms = build["ms"]
    status, log = request("GET", f"/v1/cas/{build['logs_ref']}")
    if status != 200:
        failures += 1
        print(f"add i={index} FAILED log fetch http={status} logs_ref={build['logs_ref']}")
        continue
    stages = stage_lines(log.decode(errors="replace"))
    if stages is None:
        failures += 1
        print(f"add i={index} FAILED build log {build['logs_ref']} has no build_stages line")
        continue
    walls.append(wall_ms)
    builds.append(build_ms)
    attributed = sum(v for k, v in stages.items() if k != "unattributed_ms")
    unattributed_percent = abs(build_ms - attributed) * 100 / max(build_ms, 1)
    worst_unattributed = max(worst_unattributed, unattributed_percent)
    for stage, value in stages.items():
        per_stage.setdefault(stage, []).append(value)
    ordered = " ".join(f"{k}={v}" for k, v in sorted(stages.items(), key=lambda kv: -kv[1]))
    print(
        f"add i={index} wall_ms={wall_ms:.0f} build_ms={build_ms} "
        f"rustc_invocations={build.get('rustc_invocations')} unattributed_percent={unattributed_percent:.1f} {ordered}"
    )

for stage in sorted(per_stage, key=lambda s: -statistics.median(per_stage[s])):
    print(f"stage {stage} median_ms={statistics.median(per_stage[stage]):.0f} n={len(per_stage[stage])}")
verdict = "ok" if worst_unattributed <= UNATTRIBUTED_LIMIT_PERCENT else "EXCEEDED"
print(
    f"stage-coverage worst_unattributed_percent={worst_unattributed:.1f} "
    f"limit_percent={UNATTRIBUTED_LIMIT_PERCENT} {verdict}"
)
if failures or not walls:
    print(f"add-latency FAILED failures={failures} n={count}")
    sys.exit(1)
print(
    f"add-latency median_wall_ms={statistics.median(walls):.0f} "
    f"median_build_ms={statistics.median(builds):.0f} n={len(walls)}"
)
PY

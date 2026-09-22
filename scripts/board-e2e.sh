#!/usr/bin/env bash
# Live board acceptance: one fresh daemon serving ui/build, one real browser (agent-browser),
# the CLI mutating state, and the board expected to reflect each mutation within 2 s.
# Prints `ok <n> <name>` / `FAIL <n> <name>: <reason>` and a final `N/M`; exit 0 only on M/M.
set -u
cd "$(dirname "$0")/.."
ROOT=$PWD
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
LOOMD=${LOOMD:-$TARGET/debug/loomd}
LOOM=${LOOM:-$TARGET/debug/loom}
PORT=${BOARD_E2E_PORT:-8798}
SESSION=board-e2e-$$
D=$(mktemp -d "${TMPDIR:-/tmp}/board-e2e.XXXXXX")
export LOOM_TOKEN=board-token
URL=http://127.0.0.1:$PORT
export RUSTC=${RUSTC:-$HOME/.rustup/toolchains/nightly-2026-08-24-aarch64-apple-darwin/bin/rustc}
export LOOM_HASH_RUSTC=${LOOM_HASH_RUSTC:-$ROOT/tools/hash-rustc/target/release/hash-rustc}
pass=0; total=0
check() { total=$((total+1)); if [ "$1" = 0 ]; then pass=$((pass+1)); echo "ok $total $2"; else echo "FAIL $total $2: $3"; fi; }
for bin in "$LOOMD" "$LOOM"; do [ -x "$bin" ] || { echo "missing executable $bin"; echo "0/1"; exit 1; }; done
[ -f "$ROOT/ui/build/index.html" ] || { echo "ui/build missing: run bun run build in ui/"; echo "0/1"; exit 1; }
command -v agent-browser >/dev/null || { echo "agent-browser is required"; echo "0/1"; exit 1; }
mkdir -p "$D/actors" "$D/build"
LOOM_BUILD_DIR=$D/build "$LOOMD" --db "$D/loom.sqlite" --actors-dir "$D/actors" --bind 127.0.0.1:$PORT --token "$LOOM_TOKEN" --root "$ROOT" > "$D/loomd.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null; agent-browser --session $SESSION close >/dev/null 2>&1; echo "state: $D"' EXIT
for i in $(seq 1 100); do curl -s -o /dev/null "$URL/health" && break; sleep 0.1; done
kill -0 $DPID 2>/dev/null || { echo "daemon exited at start (port $PORT busy?): $(tail -2 "$D/loomd.log")"; echo "0/1"; exit 1; }
L() { "$LOOM" --url "$URL" "$@"; }
J() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
B() { agent-browser --session "$SESSION" "$@"; }
# wait_dom <js expression returning truthy> <seconds>: polls the page until the expression holds.
wait_dom() { local expr=$1 limit=${2:-2}; local t0=$(python3 -c 'import time;print(time.time())'); while :; do
  local v; v=$(B eval "$expr" 2>/dev/null | tr -d '"\n'); [ "$v" = "true" ] && { echo "$(python3 -c "import time;print(int((time.time()-$t0)*1000))")"; return 0; }
  python3 -c "import time,sys; sys.exit(0 if time.time()-$t0 < $limit else 1)" || { echo "timeout"; return 1; }; sleep 0.1; done; }

# 1 the board loads live
B open "$URL/#token=$LOOM_TOKEN" >/dev/null 2>&1
ms=$(wait_dom "document.querySelector('[data-testid=board-status]')?.textContent.includes('live')" 10); check $? "board loads and reports live (${ms} ms)" "status: $(B eval "document.querySelector('[data-testid=board-status]')?.textContent" 2>&1 | head -c 200)"
frag=$(B eval "location.hash" 2>/dev/null | tr -d '"'); [ -z "$frag" ]; check $? "token fragment consumed from the URL" "hash still [$frag]"

# 2 a definition added by the CLI appears without reload
t0=$(python3 -c 'import time;print(time.time())')
out=$(L add examples/unison/greet.rs --lang rust --name greet 2>&1); rc=$?
check $rc "cli add greet (cold build $(python3 -c "import time;print(int(time.time()-$t0))") s)" "$(echo "$out" | head -c 300)"
ghash=$(echo "$out" | J "d['result']['def']['hash']" 2>/dev/null)
ms=$(wait_dom "!!document.querySelector('[data-testid=board-def][data-name=\"greet\"]')" 2); check $? "greet appears in the definitions pane within 2 s (${ms} ms)" "not found"
ms=$(wait_dom "!!document.querySelector('[data-testid=board-build][data-definition=\"$ghash\"] [data-stage]')" 5); check $? "build row for greet shows stage bars (${ms} ms)" "no stage bars"

# 3 static link edge and isolated edge in the graph
cat > "$D/shapes.rs" <<'EOF'
pub mod shapes {
    pub trait Area { fn area(&self) -> f64; }
    pub fn largest<T: Area + Copy>(items: Vec<T>) -> T {
        let mut best = items[0];
        for item in items.into_iter().skip(1) { if item.area() > best.area() { best = item; } }
        best
    }
}
pub fn ping() -> u32 { 1 }
EOF
cat > "$D/pick.rs" <<'EOF'
use shapes::shapes::{Area, largest};
#[derive(Clone, Copy)]
struct Square(f64);
impl Area for Square { fn area(&self) -> f64 { self.0 * self.0 } }
pub fn pick(sides: Vec<f64>) -> f64 { largest(sides.into_iter().map(Square).collect()).area() }
EOF
shash=$(L add "$D/shapes.rs" --lang rust --name shapes 2>/dev/null | J "d['result']['def']['hash']")
out=$(L add "$D/pick.rs" --lang rust --name pick --dep shapes=shapes 2>&1); rc=$?; check $rc "cli add pick --dep shapes=shapes" "$(echo "$out" | head -c 300)"
phash=$(echo "$out" | J "d['result']['def']['hash']" 2>/dev/null)
ms=$(wait_dom "!!document.querySelector('[data-kind=static][data-from=\"$phash\"][data-to=\"$shash\"]')" 3); check $? "graph draws a solid static edge pick -> shapes (${ms} ms)" "edge missing"
out=$(L add examples/rust-recursive/src/lib.rs --lang rust --name descend 2>&1); rc=$?; check $rc "cli add descend (isolated::call)" "$(echo "$out" | head -c 300)"
dhash=$(echo "$out" | J "d['result']['def']['hash']" 2>/dev/null)
ms=$(wait_dom "!!document.querySelector('[data-kind=isolated][data-from=\"$dhash\"]')" 3); check $? "graph draws a dashed isolated edge from descend (${ms} ms)" "edge missing"

# 4 a run shows up in the feed with its outcome and timing
before=$(B eval "document.querySelectorAll('[data-testid=board-event][data-type=call_completed]').length" 2>/dev/null | tr -d '"')
out=$(L run pick '[[1.5, 3.0, 2.0]]' 2>&1); rc=$?; check $rc "cli run pick" "$(echo "$out" | head -c 200)"
ms=$(wait_dom "document.querySelectorAll('[data-testid=board-event][data-type=call_completed]').length > ${before:-0}" 2); check $? "run appears in the feed within 2 s (${ms} ms)" "no new call_completed row"
row=$(B eval "document.querySelector('[data-testid=board-event][data-type=call_completed]')?.textContent" 2>/dev/null); echo "$row" | grep -q 'pick' && echo "$row" | grep -Eq '[0-9]+ ?ms'; check $? "feed row names pick and shows elapsed ms" "row: $(echo "$row" | head -c 200)"

# 5 actors: spawn and two messages move the visible cursor
L add examples/unison/counter.rs --lang rust --name counter >/dev/null 2>&1
aid=$(L spawn counter 2>/dev/null | J "d['result']['id'] if isinstance(d.get('result'),dict) and 'id' in d['result'] else d['result']" 2>/dev/null | tr -d '"')
[ -n "$aid" ]; check $? "cli spawn counter" "no actor id"
ms=$(wait_dom "!!document.querySelector('[data-testid=board-actor][data-id=\"$aid\"]')" 3); check $? "actor appears in the tree (${ms} ms)" "row missing"
L send "$aid" 1 >/dev/null 2>&1; L send "$aid" 1 >/dev/null 2>&1
ms=$(wait_dom "document.querySelector('[data-testid=board-actor][data-id=\"$aid\"]')?.dataset.cursor === '2'" 2); check $? "actor cursor reads 2 within 2 s (${ms} ms)" "cursor: $(B eval "document.querySelector('[data-testid=board-actor][data-id=\"$aid\"]')?.dataset.cursor" 2>/dev/null)"
ms=$(wait_dom "document.querySelectorAll('[data-testid=board-event][data-type=actor_message]').length >= 2" 2); check $? "two actor_message events in the feed (${ms} ms)" "count: $(B eval "document.querySelectorAll('[data-testid=board-event][data-type=actor_message]').length" 2>/dev/null)"

# 6 click-through: the definition detail is source first, formatted, with dependencies as links
B eval "document.querySelector('[data-testid=board-def][data-name=\"pick\"]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "document.querySelector('[data-testid=board-detail]')?.dataset.hash === '$phash'" 3); check $? "clicking pick opens its detail (${ms} ms)" "detail: $(B eval "document.querySelector('[data-testid=board-detail]')?.dataset.hash" 2>/dev/null)"
ms=$(wait_dom "(document.querySelector('[data-testid=detail-source]')?.textContent ?? '').includes('largest(sides')" 3); check $? "detail shows pick's source (${ms} ms)" "source missing"
ms=$(wait_dom "document.querySelectorAll('[data-testid=detail-source] [data-line]').length >= 8" 3); v=$(B eval "document.querySelectorAll('[data-testid=detail-source] [data-line]').length" 2>/dev/null | tr -d '"'); [ "${v:-0}" -ge 8 ]; check $? "source is rustfmt-formatted: pick.rs was 6 lines, shown as $v (the one-line body was split, ${ms} ms)" "lines: $v"
v=$(B eval "location.hash" 2>/dev/null | tr -d '"'); [ "$v" = "#def=$phash" ]; check $? "URL hash deep-links the selection" "hash [$v]"
B eval "document.querySelector('[data-testid=detail-dep][data-hash=\"$shash\"]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "document.querySelector('[data-testid=board-detail]')?.dataset.hash === '$shash' && (document.querySelector('[data-testid=detail-source]')?.textContent ?? '').includes('pub fn largest')" 3); check $? "dependency link navigates to shapes' source (${ms} ms)" "not navigated"

# 7 wasm tab with source mapping
B eval "document.querySelector('[data-tab=wasm]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "document.querySelectorAll('[data-testid=detail-wasm] [data-wat-line]').length > 100" 10); check $? "wasm text renders (${ms} ms)" "lines: $(B eval "document.querySelectorAll('[data-testid=detail-wasm] [data-wat-line]').length" 2>/dev/null)"
v=$(B eval "document.querySelectorAll('[data-testid=detail-wasm] [data-src-line][data-src-file=\"src/lib.rs\"]').length" 2>/dev/null | tr -d '"'); [ "${v:-0}" -gt 0 ]; check $? "wasm lines map to src/lib.rs lines via DWARF ($v mapped)" "mapped: $v"
wait_dom "document.querySelectorAll('[data-testid=wasm-source] [data-line]').length > 0" 5 >/dev/null
first=$(B eval "(() => { const shown = document.querySelectorAll('[data-testid=wasm-source] [data-line]').length; const lines = [...document.querySelectorAll('[data-testid=detail-wasm] [data-src-line][data-src-file=\"src/lib.rs\"]')].map(e => Number(e.dataset.srcLine)).filter(n => n <= shown); return lines.length ? Math.min(...lines) : ''; })()" 2>/dev/null | tr -d '"')
B eval "document.querySelector('[data-testid=wasm-source] [data-line=\"$first\"]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "document.querySelectorAll('[data-testid=detail-wasm] [data-src-line=\"$first\"].hot').length > 0" 2); check $? "clicking source line $first lights its wasm instructions (${ms} ms)" "no hot wat lines"
B eval "document.querySelector('[data-testid=detail-back]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "!document.querySelector('[data-testid=board-detail]') && !!document.querySelector('[data-kind=static]')" 2); check $? "back returns to the overview (${ms} ms)" "detail still open"

# 8 actor and run details
B eval "document.querySelector('[data-testid=board-actor][data-id=\"$aid\"]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "document.querySelector('[data-testid=detail-actor]')?.dataset.id === '$aid' && (document.querySelector('[data-testid=detail-actor]')?.textContent ?? '').includes('counter')" 3); check $? "actor detail names its definition (${ms} ms)" "missing"
B eval "document.querySelector('[data-testid=detail-back]').click(); 'ok'" >/dev/null 2>&1
B eval "document.querySelector('[data-testid=board-event][data-type=call_completed]').click(); 'ok'" >/dev/null 2>&1
ms=$(wait_dom "(document.querySelector('[data-testid=detail-run]')?.textContent ?? '').includes('pick')" 3); check $? "run detail opens from the feed (${ms} ms)" "missing"
B eval "document.querySelector('[data-testid=detail-back]').click(); 'ok'" >/dev/null 2>&1

# 9 the workspace hides its parameter dumps by default
B open "$URL/workspace" >/dev/null 2>&1
ms=$(wait_dom "!!document.body && document.body.textContent.length > 0" 5) >/dev/null
v=$(B eval "document.body.textContent.includes('order_by=')" 2>/dev/null | tr -d '"'); [ "$v" = "false" ]; check $? "workspace shows no parameter dump by default" "order_by= visible"
B open "$URL/#token=$LOOM_TOKEN" >/dev/null 2>&1; wait_dom "!!document.querySelector('[data-testid=board-status]')" 5 >/dev/null

# 10 no console errors, and the old routes still work
errs=$(B errors 2>&1 | grep -vE '^\s*$' | wc -l | tr -d ' '); [ "$errs" = 0 ]; check $? "no browser console errors" "$(B errors --json 2>&1 | head -c 600)"
code=$(curl -sL -o /dev/null -w '%{http_code}' "$URL/workspace"); [ "$code" = 200 ]; check $? "/workspace serves the app (SPA fallback, redirects followed)" "http $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "$URL/view"); [ "$code" = 200 ]; check $? "/view serves the app (SPA fallback)" "http $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "$URL/missing.js"); [ "$code" = 404 ]; check $? "/missing.js stays 404" "http $code"
B screenshot "$D/board.png" >/dev/null 2>&1; [ -s "$D/board.png" ]; check $? "screenshot saved at $D/board.png" "no file"

echo "$pass/$total"; [ "$pass" = "$total" ]

#!/usr/bin/env bash
# Run against one isolated production daemon; build loomd before invoking this gate.
set -euo pipefail
cd "$(dirname "$0")/.."
ui_root=$PWD
ui_binary=${LOOMD:-${CARGO_TARGET_DIR:-target}/debug/loomd}
if [[ ! -x "$ui_binary" ]]; then
  printf 'Missing executable LOOMD: %s\n' "$ui_binary" >&2
  printf '0/4\n'
  exit 1
fi
if ! command -v bun >/dev/null; then
  printf 'bun is required\n' >&2
  printf '0/4\n'
  exit 1
fi
ui_scratch=$(mktemp -d "$ui_root/.ui-e2e.XXXXXX")
ui_pid=''
cleanup() {
  if [[ -n "$ui_pid" ]]; then
    kill -INT "$ui_pid" 2>/dev/null || true
    wait "$ui_pid" 2>/dev/null || true
  fi
  rm -rf "$ui_scratch"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The copied executable remains fixed even if the consolidated gate rebuilds loomd.
cp "$ui_binary" "$ui_scratch/loomd"
export LOOM_TOKEN="ui-e2e-$RANDOM-$RANDOM"
export LOOM_URL="http://127.0.0.1:${LOOM_UI_PORT:-18789}"
export LOOM_UI_LOG="$ui_scratch/daemon.log"
"$ui_scratch/loomd" --root "$ui_root" --db "$ui_scratch/loom.sqlite" \
  --actors-dir "$ui_scratch/actors" --bind "127.0.0.1:${LOOM_UI_PORT:-18789}" \
  >"$ui_scratch/daemon.log" 2>&1 &
ui_pid=$!
cat > "$ui_scratch/probe.ts" <<'TS'
const endpoint = process.env.LOOM_URL!;
const token = process.env.LOOM_TOKEN!;
type ObjectValue = Record<string, unknown>;
interface Definition { hash: string; def: { component_hash: string } }
interface ViewResult { id: string; cap: unknown }
interface DeltaRow { change_id: number; change_type: number; after: { key: string; tree: number[] } }
interface Delta { type: string; source: string; key: string; causation?: string; rows: DeltaRow[] }
let passed = 0;
let socket: WebSocket | undefined;
let failure: Error | undefined;
let closed = false;
const frames: ObjectValue[] = [];
function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
async function until(predicate: () => boolean, label: string, timeout = 15_000) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    if (failure) throw failure;
    assert(Date.now() < deadline, `timeout: ${label}`);
    await Bun.sleep(10);
  }
}
async function frame(predicate: (value: ObjectValue) => boolean, label: string): Promise<ObjectValue> {
  await until(() => frames.some(predicate), label);
  return frames.splice(frames.findIndex(predicate), 1)[0]!;
}
async function command<T>(name: string, args: ObjectValue): Promise<T> {
  const response = await fetch(`${endpoint}/v1/command`, {
    method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ command: name, args }), signal: AbortSignal.timeout(name === 'add' ? 600_000 : 30_000),
  });
  const result = await response.json() as { ok: boolean; result: T; diagnostics: unknown[] };
  assert(response.ok && result.ok, `${name}: HTTP ${response.status}: ${JSON.stringify(result)}`);
  return result.result;
}
async function check<T>(name: string, body: () => Promise<T>): Promise<T> {
  const result = await body();
  passed++;
  console.log(`PASS ${name}`);
  return result;
}
const counterSource = `
pub const LOOM_SCHEMA: &str = "CREATE TABLE entries(seq INTEGER, body TEXT)";
pub fn message(msg: Vec<u8>) {
    let msg: loom::Value = loom::serde_json::from_slice(&msg).unwrap();
    let request = loom::serde_json::json!({
        "sql":"INSERT INTO entries(seq,body) VALUES (?,?)",
        "params":[{"type":"integer","value":msg["seq"]},{"type":"text","value":msg["text"]}]
    });
    loom::perform::<loom::Value>("sql", request).unwrap();
}`;
function templateSource(version: string): string {
  return `pub fn render(row: loom::Value) -> loom::Value {
    loom::serde_json::json!({"tag":"li","key":format!("row-{}", row["seq"]),
      "attrs":{"class":"${version}"},"children":[row["body"].as_str().unwrap().to_owned()]})
  }`;
}
try {
  const ready = Date.now() + 30_000;
  while (true) {
    // Require this daemon's successful bind, so an occupied port cannot test another store.
    const daemonLog = await Bun.file(process.env.LOOM_UI_LOG!).text();
    if (daemonLog.includes(`loomd listening on ${new URL(endpoint).host}`)) {
      try { if ((await fetch(`${endpoint}/health`, { signal: AbortSignal.timeout(500) })).ok) break; } catch {}
    }
    assert(Date.now() < ready, 'daemon did not become ready');
    await Bun.sleep(100);
  }
  const setup = await check('real counter and pure view produce a WebSocket snapshot', async () => {
    const counter = await command<Definition>('add', { name: 'ui-e2e-counter', source: counterSource });
    const first = await command<Definition>('add', { name: 'ui-e2e-template-a', source: templateSource('template-a') });
    const second = await command<Definition>('add', { name: 'ui-e2e-template-b', source: templateSource('template-b') });
    assert(first.hash !== second.hash, 'different template content must have different identities');
    const source = (await command<{ id: string }>('spawn', { def: counter.hash, init: null })).id;
    const view = await command<ViewResult>('view', { actor: source, table: 'entries', template: first.hash, order_by: ['seq'] });
    assert(view.id && view.cap, 'view must return actor identity and INSPECT capability');
    socket = new WebSocket(`${endpoint.replace('http:', 'ws:')}/v1/stream`);
    socket.addEventListener('error', () => { failure = new Error('WebSocket transport error'); });
    socket.addEventListener('close', () => { closed = true; });
    socket.addEventListener('message', (event) => {
      try {
        const value: unknown = JSON.parse(String(event.data));
        assert(value !== null && typeof value === 'object' && !Array.isArray(value), 'non-object stream frame');
        const message = value as ObjectValue;
        assert(!message.error, `stream: ${String(message.error)}`);
        frames.push(message);
      } catch (error) { failure = error instanceof Error ? error : new Error(String(error)); }
    });
    await until(() => socket!.readyState === WebSocket.OPEN, 'WebSocket open');
    socket.send(JSON.stringify({ token, after: 0 }));
    await frame((value) => value.ok === true, 'authentication acknowledgement');
    socket.send(JSON.stringify({ subscribe: { actor: view.id, table: 'tree', cap: view.cap } }));
    const snapshot = await frame((value) => value.type === 'snapshot' && value.source === view.id, 'initial snapshot');
    assert(Array.isArray(snapshot.rows) && snapshot.rows.length === 0, 'initial view snapshot must be empty');
    assert(typeof snapshot.change_id === 'number', 'snapshot must carry CDC high-water mark');
    return { source, view, second };
  });
  const { source, view, second } = setup;
  const liveKeys = new Set<string>();
  await check('three sends produce three distinct keyed tree deltas', async () => {
    let previousChange = -1;
    for (let seq = 1; seq <= 3; seq++) {
      const key = `ui-e2e-message-${seq}`;
      await command('send', { id: source, key, msg: { seq, text: `row ${seq}` } });
      const delta = await frame((value) => value.type === 'delta' && value.source === view.id, `delta ${seq}`) as unknown as Delta;
      assert(delta.rows.length === 1, `send ${seq} must change exactly one tree row`);
      assert(delta.key.length > 0 && delta.causation === key, `send ${seq} lost exact key or originating message key`);
      const row = delta.rows[0]!;
      assert(row.change_type === 1 && row.after.key === String(seq), `send ${seq} inserted the wrong key`);
      assert(row.change_id > previousChange, 'CDC change IDs must increase');
      previousChange = row.change_id;
      liveKeys.add(row.after.key);
    }
    assert(liveKeys.size === 3, 'three sends must create three keys');
    await command('drain', {});
    await Bun.sleep(150);
    assert(!frames.some((value) => value.type === 'delta' && value.source === view.id), 'duplicate send delta');
  });
  await check('promote emits one transaction updating every existing key', async () => {
    await command('promote', { id: view.id, hash: second.hash, author: 'ui-e2e', rationale: 'template reload' });
    await command('drain', {});
    const delta = await frame((value) => value.type === 'delta' && value.source === view.id, 'promotion delta') as unknown as Delta;
    assert(delta.rows.length === 3, 'promotion must update all three rows in one frame');
    const updated = new Set<string>();
    for (const row of delta.rows) {
      assert(row.change_type === 0 && liveKeys.has(row.after.key), 'promotion replaced a live key');
      const tree = JSON.parse(new TextDecoder().decode(Uint8Array.from(row.after.tree))) as { attrs: { class: string } };
      assert(tree.attrs.class === 'template-b', 'promotion did not execute the second template');
      updated.add(row.after.key);
    }
    assert(updated.size === 3, 'promotion duplicated a key');
    await Bun.sleep(150);
    assert(!frames.some((value) => value.type === 'delta' && value.source === view.id), 'promotion emitted multiple frames');
  });
  await check('immutable CAS and socket close remove the host subscription', async () => {
    const cas = await fetch(`${endpoint}/v1/cas/${second.def.component_hash}`, { headers: { Authorization: `Bearer ${token}` } });
    assert(cas.ok, `CAS HTTP ${cas.status}`);
    assert(cas.headers.get('Cache-Control') === 'public, max-age=31536000, immutable', 'CAS cache policy differs');
    await cas.arrayBuffer();
    const active = await command<{ subscriber: string }[]>('subscriptions', { id: view.id });
    assert(active.length === 1 && active[0]!.subscriber.startsWith('ws:'), 'expected one ws subscriber');
    socket!.close();
    await until(() => closed, 'socket close');
    const deadline = Date.now() + 15_000;
    while ((await command<unknown[]>('subscriptions', { id: view.id })).length !== 0) {
      assert(Date.now() < deadline, 'socket close retained subscriber rows');
      await Bun.sleep(20);
    }
  });
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  socket?.close();
  console.log(`${passed}/4`);
}
TS
if ! bun "$ui_scratch/probe.ts"; then
  cat "$ui_scratch/daemon.log" >&2
  exit 1
fi

/** Run only against an isolated daemon: this creates versioned test definitions. */
import { readFile } from 'node:fs/promises';

type ObjectValue = Record<string, unknown>;
function object(value: unknown): ObjectValue {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`Expected object: ${JSON.stringify(value)}`);
  return value as ObjectValue;
}
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
function equal(actual: unknown, expected: unknown, message: string) {
  assert(JSON.stringify(actual) === JSON.stringify(expected), `${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}
function hash(view: ObjectValue): string {
  assert(typeof view.hash === 'string' && /^[a-f0-9]{64}$/.test(view.hash), 'Missing definition hash');
  return view.hash;
}
function entryHash(view: ObjectValue): string {
  const value = object(object(view.entries).main).hash;
  assert(typeof value === 'string' && /^[a-f0-9]{64}$/.test(value), 'Missing entry hash');
  return value;
}
const prefix = `evolution_${Date.now()}_${process.pid}`;
const names = { base: `${prefix}_base`, caller: `${prefix}_caller`, outer: `${prefix}_outer`, unrelated: `${prefix}_unrelated`, config: `${prefix}_config` };
let endpoint = '';
let token = '';
let timeout = 180000;
async function raw(command: string, args: ObjectValue): Promise<ObjectValue> {
  console.log(`# ${command} ${JSON.stringify(args)}`);
  const response = await fetch(`${endpoint}/v1/command`, {
    method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ command, args }), signal: AbortSignal.timeout(timeout),
  });
  const body = await response.text();
  assert(response.ok, `${command}: HTTP ${response.status}: ${body}`);
  const envelope = object(JSON.parse(body));
  assert(typeof envelope.ok === 'boolean' && Array.isArray(envelope.diagnostics), `${command}: invalid envelope`);
  return envelope;
}
async function call(command: string, args: ObjectValue): Promise<ObjectValue> {
  const envelope = await raw(command, args);
  assert(envelope.ok === true, `${command}: ${JSON.stringify(envelope)}`);
  return object(envelope.result);
}
async function view(target: string) { return call('view', { target }); }
async function run(target: string, output: unknown) { equal((await call('run', { target, args: [] })).output, output, `run ${target}`); }
function session(result: ObjectValue): ObjectValue { return object(result.update ?? result); }
async function unchanged(before: Record<string, ObjectValue>) {
  for (const [name, definition] of Object.entries(before)) equal(hash(await view(name)), hash(definition), `${name} identity`);
}
let original: Record<string, ObjectValue> = {};
let compatible: Record<string, ObjectValue> = {};
let pending: ObjectValue = {};
const baseSource = 'pub fn main() -> i32 { 41 }';
const changedSource = 'pub fn main() -> i32 { 42 }';
const incompatibleSource = 'pub fn main() -> String { let _configured = configured::main(); String::from("42") }';
const repairChanges = () => ({
  [names.base]: { source: incompatibleSource },
  [names.caller]: { source: 'pub fn main() -> String { base::main() + "!" }' },
  [names.outer]: { source: 'pub fn main() -> String { caller::main() + "?" }' },
});
const gates = [
  { name: 'native add and execution', check: async () => {
    endpoint = (process.env.LOOM_URL ?? '').replace(/\/$/, '');
    assert(endpoint, 'Set LOOM_URL to an isolated daemon');
    const tokenFile = process.env.LOOM_TOKEN_FILE;
    token = tokenFile ? (await readFile(tokenFile, 'utf8')).trim() : process.env.LOOM_TOKEN ?? '';
    timeout = Number(process.env.LOOM_E2E_OPERATION_TIMEOUT_MS ?? 180000);
    assert(Number.isSafeInteger(timeout) && timeout > 0, 'Invalid timeout');
    original[names.base] = await call('add', { name: names.base, source: baseSource });
    hash(original[names.base]!); entryHash(original[names.base]!);
    await run(names.base, 41);
  } },
  { name: 'compatible transitive propagation', check: async () => {
    original[names.caller] = await call('add', { name: names.caller, source: 'pub fn main() -> i32 { base::main() + 1 }', deps: { base: hash(original[names.base]!) } });
    original[names.outer] = await call('add', { name: names.outer, source: 'pub fn main() -> i32 { caller::main() * 2 }', deps: { caller: hash(original[names.caller]!) } });
    original[names.config] = await call('add', { name: names.config, source: 'pub fn main() -> i32 { 123 }' });
    original[names.unrelated] = await call('add', { name: names.unrelated, source: 'pub fn main() -> i32 { 777 }' });
    await run(names.outer, 84);
    const request = { name: names.base, source: changedSource, expected_hash: hash(original[names.base]!), request_id: `${prefix}_request` };
    const updated = await call('update', request);
    equal(session(await call('update', request)), session(updated), 'initial request replay');
    assert((await raw('update', { ...request, source: baseSource })).ok === false, 'request id must reject different inputs');
    equal(session(updated).status, 'complete', 'compatible update status');
    for (const name of [names.base, names.caller, names.outer]) {
      compatible[name] = await view(name);
      assert(hash(compatible[name]!) !== hash(original[name]!), `${name} did not change hash`);
      assert(entryHash(compatible[name]!) !== entryHash(original[name]!), `${name} did not change entry hash`);
    }
    await run(names.base, 42); await run(names.caller, 43); await run(names.outer, 86);
  } },
  { name: 'old definition and entry hashes execute', check: async () => {
    for (const test of [{ name: names.base, output: 41 }, { name: names.caller, output: 42 }, { name: names.outer, output: 84 }]) {
      await run(hash(original[test.name]!), test.output); await run(entryHash(original[test.name]!), test.output);
    }
  } },
  { name: 'unrelated and no-op identities preserved', check: async () => {
    await unchanged({ [names.unrelated]: original[names.unrelated]! });
    await run(names.unrelated, 777);
    await call('update', { name: names.base, source: changedSource, expected_hash: hash(compatible[names.base]!) });
    await unchanged(compatible); await run(names.outer, 86);
  } },
  { name: 'durable repair session without live mutation', check: async () => {
    pending = session(await call('update', { name: names.base, source: incompatibleSource, expected_hash: hash(compatible[names.base]!), deps: { configured: hash(original[names.config]!) }, allowed_effects: [] }));
    equal(pending.status, 'needs_repair', 'incompatible status');
    assert(typeof pending.id === 'string' && Number.isSafeInteger(pending.revision), 'Missing durable update identity');
    const reloaded = session(await call('update_view', { id: pending.id }));
    equal(reloaded, pending, 'durable session reload');
    await unchanged(compatible); await run(names.outer, 86);
  } },
  { name: 'multi-source repair commits together', check: async () => {
    const result = session(await call('update_repair', { id: pending.id, revision: pending.revision, changes: repairChanges() }));
    equal(result.status, 'complete', 'repair status');
    assert(Number(result.revision) > Number(pending.revision), 'Repair revision did not advance');
    await run(names.base, '42'); await run(names.caller, '42!'); await run(names.outer, '42!?');
    equal(object((await view(names.base)).def).allowed_effects, [], 'source-only repair retains pending policy');
    await run(hash(compatible[names.outer]!), 86);
    await unchanged({ [names.unrelated]: original[names.unrelated]! });
  } },
  { name: 'stale revision explicitly rejected', check: async () => {
    const before = await view(names.outer);
    const rejected = await raw('update_repair', { id: pending.id, revision: pending.revision, changes: repairChanges() });
    assert(rejected.ok === false && /revision|stale|conflict/i.test(JSON.stringify(rejected)), `No explicit stale revision rejection: ${JSON.stringify(rejected)}`);
    const staleHash = await raw('update', { name: names.base, source: changedSource, expected_hash: hash(original[names.base]!) });
    assert(staleHash.ok === false && /expected|stale|conflict|changed/i.test(JSON.stringify(staleHash)), `No stale expected_hash rejection: ${JSON.stringify(staleHash)}`);
    await unchanged({ [names.outer]: before }); await run(names.outer, '42!?');
  } },
  { name: 'namespace conflict, safe rebase, and edited-name refusal', check: async () => {
    const base = await view(names.base);
    const waiting = session(await call('update', { name: names.base, source: 'pub fn main() -> i32 { 90 }', expected_hash: hash(base) }));
    equal(waiting.status, 'needs_repair', 'concurrent repair setup');
    const unrelated = await view(names.unrelated);
    await call('update', { name: names.unrelated, source: 'pub fn main() -> i32 { 778 }', expected_hash: hash(unrelated) });
    const unrelatedChanged = await view(names.unrelated);
    const live = { [names.base]: await view(names.base), [names.caller]: await view(names.caller), [names.outer]: await view(names.outer) };
    const rejected = await raw('update_repair', { id: waiting.id, revision: waiting.revision, changes: {
      [names.caller]: { source: 'pub fn main() -> i32 { base::main() + 1 }' },
      [names.outer]: { source: 'pub fn main() -> i32 { caller::main() * 2 }' },
    } });
    const result = rejected.ok === true ? session(object(rejected.result)) : undefined;
    assert(result?.status === 'conflict' || (rejected.ok === false && /namespace|conflict|changed/i.test(JSON.stringify(rejected))), `No namespace conflict rejection: ${JSON.stringify(rejected)}`);
    await unchanged(live); await run(names.outer, '42!?');
    const conflicted = session(await call('update_view', { id: waiting.id }));
    equal(conflicted.status, 'conflict', 'durable namespace conflict');
    const rebased = session(await call('update_rebase', { id: conflicted.id, revision: conflicted.revision }));
    equal(rebased.status, 'complete', 'disjoint rebase status');
    await run(names.base, 90); await run(names.caller, 91); await run(names.outer, 182);
    await unchanged({ [names.unrelated]: unrelatedChanged }); await run(names.unrelated, 778);

    const currentBase = await view(names.base);
    const sameName = session(await call('update', { name: names.base, source: incompatibleSource, expected_hash: hash(currentBase) }));
    equal(sameName.status, 'needs_repair', 'same-name conflict setup');
    await call('update', { name: names.base, source: 'pub fn main() -> i32 { 91 }', expected_hash: hash(currentBase) });
    const moved = { [names.base]: await view(names.base), [names.caller]: await view(names.caller), [names.outer]: await view(names.outer) };
    const repairConflict = session(await call('update_repair', { id: sameName.id, revision: sameName.revision, changes: repairChanges() }));
    equal(repairConflict.status, 'conflict', 'edited name conflict');
    const refused = session(await call('update_rebase', { id: repairConflict.id, revision: repairConflict.revision }));
    equal(refused.status, 'conflict', 'rebase must reject changed edited name');
    assert(/rebase_conflict|edited|changed/i.test(JSON.stringify(refused.diagnostics)), 'Missing edited-name conflict diagnostic');
    await unchanged(moved); await run(names.outer, 184); await run(names.unrelated, 778);
  } },
];
let passed = 0;
let firstFailure: string | undefined;
for (const [index, gate] of gates.entries()) {
  if (firstFailure) { console.log(`BLOCKED ${index + 1} ${gate.name}`); continue; }
  try { await gate.check(); passed++; console.log(`PASS ${index + 1} ${gate.name}`); }
  catch (error) { firstFailure = `${index + 1} ${gate.name}: ${String(error).replace(/\s+/g, ' ')}`; console.error(`FAIL ${firstFailure}`); }
}
console.log(`${passed}/8${firstFailure ? `; first failure: ${firstFailure}` : ''}`);
process.exitCode = passed === 8 ? 0 : 1;

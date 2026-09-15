/** CLI and HTTP MCP proof against the shared loom-proto verb table. */
import { readFile, rename } from 'node:fs/promises';
import { join } from 'node:path';
import { LoomMcpClient, object } from './mcp-client';

const mode = process.argv[2];
const directory = process.argv[3];
if ((mode !== '--cli' && mode !== '--mcp') || !directory) {
  throw new Error('Usage: bun scripts/e2e-unison-mcp.ts --cli|--mcp <disposable-fixture-directory>');
}
const mcp = mode === '--mcp';
const prefix = mcp ? 'mcp_' : '';
const endpoint = process.env.LOOM_URL;
if (!endpoint) throw new Error('Set LOOM_URL to the daemon under test');
const client = new LoomMcpClient({
  endpoint,
  token: process.env.LOOM_TOKEN ?? '',
});
const names = ['add', 'view', 'run', 'update-history', 'inferred-sleep', 'actor-counter', 'validate', 'promote', 'tool-discovery'];
let passed = 0;
let firstFailure: number | undefined;
let greetHash = '';
let counterHash = '';
let candidateHash = '';
let actorId = '';
let counterEffects: unknown;
let connected = false;
const timeout = Number(process.env.LOOM_E2E_OPERATION_TIMEOUT_MS ?? 600000);

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
function list(value: unknown): unknown[] {
  assert(Array.isArray(value), `Expected array, got ${JSON.stringify(value)}`);
  return value;
}
function text(value: unknown): string {
  assert(typeof value === 'string' && value.length > 0, `Expected nonempty string, got ${JSON.stringify(value)}`);
  return value;
}
function equal(actual: unknown, expected: unknown, label: string) {
  assert(JSON.stringify(actual) === JSON.stringify(expected), `${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}
function effects(result: Record<string, unknown>, entry: string) {
  const row = object(object(object(result.entries)[entry]).effects);
  return { labels: list(row.labels), unknown: row.unknown };
}
function hash(value: unknown): string {
  const result = text(value);
  assert(/^[0-9a-f]{64}$/.test(result), `Expected BLAKE3 definition hash, got ${result}`);
  return result;
}
async function bounded<T>(work: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([work, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`operation timed out after ${timeout}ms`)), timeout);
    })]);
  } finally { clearTimeout(timer); }
}
async function call(cli: string[], tool: string, args: Record<string, unknown>): Promise<unknown> {
  console.log(`# request ${JSON.stringify({ command: tool, args })}`);
  let value: unknown;
  let exitStatus = 0;
  if (mcp) {
    if (!connected) { await bounded(client.connect()); connected = true; }
    const result = object(await bounded(client.rpc('tools/call', { name: tool, arguments: args })));
    assert(result.isError !== true, `${tool}: ${JSON.stringify(result)}`);
    value = object(result.structuredContent, 'MCP structured response');
  } else {
    const child = Bun.spawn(['loom', '--url', endpoint, ...cli], { stdout: 'pipe', stderr: 'pipe' });
    try {
      const output = { stdout: '', stderr: '', status: -1 };
      await bounded(Promise.all([
        new Response(child.stdout).text().then(value => { output.stdout = value; }),
        new Response(child.stderr).text().then(value => { output.stderr = value; }),
        child.exited.then(value => { output.status = value; }),
      ]));
      if (output.stderr) process.stderr.write(output.stderr);
      exitStatus = output.status;
      value = JSON.parse(output.stdout);
    } finally { child.kill(); }
  }
  console.log(`# ${tool} ${JSON.stringify(value)}`);
  const envelope = object(value);
  assert(Number.isSafeInteger(envelope.seq) && Array.isArray(envelope.diagnostics), `${tool}: invalid response envelope`);
  assert(envelope.ok === true, `${tool}: ${JSON.stringify(envelope)}`);
  assert(exitStatus === 0, `loom ${cli[0]} exited ${exitStatus}`);
  assert('result' in envelope, `${tool}: missing result`);
  return envelope.result;
}
async function add(file: string, name: string): Promise<Record<string, unknown>> {
  const path = join(directory, file);
  const source = await readFile(path, 'utf8');
  assert(!source.includes('#[') && !/\w+!\s*\(/.test(source), `${file}: guest must have no macros`);
  return object(await call(['add', path, '--name', name, '--lang', 'rust'], 'add', { lang: 'rust', source, name }));
}
async function runGreeting(reference: string, expected: string) {
  const result = object(await call(['run', reference, '"loom"'], 'run', { target: reference, args: 'loom' }));
  equal(result.output, expected, 'greeting value');
  equal(result.effects, [], 'run effects');
}
async function cursor(expected: number) {
  const result = object(await call(['info', actorId], 'info', { id: actorId }));
  equal(result.cursor, expected, 'actor cursor');
}
async function send() {
  await call(['send', actorId, '1'], 'send', { id: actorId, msg: 1 });
}

const checks: Array<() => Promise<void>> = [
  async () => {
    const result = await add('greet.rs', `${prefix}greet`);
    greetHash = hash(result.hash);
    equal(effects(result, 'greet'), { labels: [], unknown: false }, 'inferred effect row');
  },
  async () => {
    const path = join(directory, 'greet.rs');
    const source = await readFile(path, 'utf8');
    await rename(path, `${path}.removed`);
    try {
      const result = object(await call(['view', greetHash], 'view', { target: greetHash }));
      equal(result.source, source, 'stored source after input removal');
      equal(Object.keys(object(result.items)).length, 2, 'item table count');
    } finally { await rename(`${path}.removed`, path); }
  },
  async () => { await runGreeting(`${prefix}greet`, 'hello, loom'); },
  async () => {
    const name = `${prefix}greet`;
    const update = async (file: string) => object(await call(
      ['update', name, join(directory, file)], 'update',
      { name, source: await readFile(join(directory, file), 'utf8') },
    ));
    equal(hash((await update('greet-v2.rs')).hash), greetHash, 'alpha-equivalent hash');
    const changedHash = hash((await update('greet-v3.rs')).hash);
    assert(changedHash !== greetHash, 'constant change did not move definition hash');
    await runGreeting(greetHash, 'hello, loom');
    await runGreeting(name, 'welcome, loom');
    const history = list(await call(['history', name], 'history', { name }));
    const hashes = history.map(item => hash(object(item).hash));
    assert(hashes.includes(greetHash) && hashes.includes(changedHash), 'history lacks old or new hash');
  },
  async () => {
    const source = await readFile(join(directory, 'sleeper.rs'), 'utf8');
    assert(!/LOOM_EFFECT|effects\s*=|#\[/.test(source), 'sleeper declares an effect row');
    const result = await add('sleeper.rs', `${prefix}sleeper`);
    hash(result.hash);
    equal(effects(result, 'sleeper'), { labels: ['sleep'], unknown: false }, 'generic trait inferred row');
  },
  async () => {
    const result = await add('counter.rs', `${prefix}counter`);
    counterHash = hash(result.hash);
    counterEffects = effects(result, 'handle');
    equal(counterEffects, { labels: ['sql'], unknown: false }, 'counter inferred row');
    const spawned = object(await call(['spawn', `${prefix}counter`], 'spawn', { def: `${prefix}counter`, init: null }));
    actorId = text(spawned.id);
    await send(); await send(); await send();
    await cursor(3);
  },
  async () => {
    const result = await add('counter-v2.rs', `${prefix}counter-v2`);
    candidateHash = hash(result.hash);
    assert(candidateHash !== counterHash, 'counter revision hash did not change');
    equal(effects(result, 'handle'), counterEffects, 'counter revisions inferred rows');
    const validation = object(await call(['validate', actorId, candidateHash, '3'], 'validate', {
      id: actorId, candidate: candidateHash, k: 3,
    }));
    const differences = list(object(object(validation.verdict).Differs).tables).map(item => object(item));
    const counter = differences.find(item => item.name === 'counter');
    assert(counter, 'Differs verdict lacks counter table');
    const original = hash(counter.original_hash);
    const fork = hash(counter.fork_hash);
    assert(original !== fork, 'Differs table hashes are equal');
    await cursor(3);
  },
  async () => {
    await call(['promote', actorId, candidateHash, '--rationale', 'e2e', '--author', 'e2e'], 'promote', {
      id: actorId, hash: candidateHash, author: 'e2e', rationale: 'e2e',
    });
    const lineage = list(await call(['lineage', actorId], 'lineage', { id: actorId }));
    const hashes = lineage.map(item => text(object(item).behavior_hash));
    assert(hashes.includes(counterHash) && hashes.includes(candidateHash), 'lineage lacks both behavior hashes');
    await send();
    await cursor(4);
  },
  async () => {
    const discovery = object(await bounded(client.rpc('tools/list')));
    const tools = list(discovery.tools).map(item => text(object(item).name));
    const definitions = ['add', 'view', 'update', 'history', 'diff', 'run', 'find', 'dependents'];
    const actors = ['spawn', 'send', 'tree', 'info', 'lineage', 'validate', 'promote', 'fork', 'actors',
      'stop', 'restart', 'dead_letters', 'sql', 'whereis', 'register', 'members', 'behaviors', 'promote_where', 'drain'];
    equal(tools.sort(), ['command', ...definitions, ...actors].sort(), 'shared verb discovery');
  },
];

try {
  for (let index = 0; index < (mcp ? 9 : 8); index++) {
    const step = index + 1;
    if (firstFailure !== undefined) {
      console.log(`FAIL ${step} ${names[index]}: blocked by step ${firstFailure}`);
      continue;
    }
    try {
      await checks[index]!();
      passed++;
      console.log(`ok ${step} ${names[index]}`);
    } catch (error) {
      firstFailure = step;
      console.log(`FAIL ${step} ${names[index]}: ${String(error).replace(/\s+/g, ' ')}`);
    }
  }
} finally {
  if (connected) {
    try { await bounded(client.close()); }
    catch (error) { console.error(`MCP close: ${String(error)}`); process.exitCode = 1; }
  }
  if (mcp) console.log(`${passed}/9`);
  if (firstFailure !== undefined) process.exitCode = 1;
}
// A timed-out HTTP request may still own a socket. All verdicts are printed;
// terminate this client so the shell can stop its owned daemon.
process.exit(process.exitCode ?? 0);

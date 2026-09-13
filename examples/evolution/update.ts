/** Scriptable optimistic updates. See README.md for command examples. */
import { readFile } from 'node:fs/promises';

type JsonObject = Record<string, unknown>;
function object(value: unknown): JsonObject {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Expected JSON object');
  return value as JsonObject;
}
const endpoint = process.env.LOOM_URL?.replace(/\/$/, '');
if (!endpoint) throw new Error('Set LOOM_URL');
const token = process.env.LOOM_TOKEN_FILE
  ? (await readFile(process.env.LOOM_TOKEN_FILE, 'utf8')).trim()
  : process.env.LOOM_TOKEN ?? '';
const timeout = Number(process.env.LOOM_UPDATE_TIMEOUT_MS ?? 600000);
if (!Number.isSafeInteger(timeout) || timeout <= 0) throw new Error('Invalid LOOM_UPDATE_TIMEOUT_MS');
async function command(command: string, args: JsonObject): Promise<JsonObject> {
  const response = await fetch(`${endpoint}/v1/command`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ command, args }),
    signal: AbortSignal.timeout(timeout),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${text}`);
  const envelope = object(JSON.parse(text));
  if (envelope.ok !== true) throw new Error(JSON.stringify(envelope));
  return object(envelope.result);
}
function required(value: string | undefined, name: string): string {
  if (!value) throw new Error(`Missing ${name}`);
  return value;
}
function revision(value: string | undefined): number {
  const result = Number(required(value, 'revision'));
  if (!Number.isSafeInteger(result) || result < 0) throw new Error('Invalid revision');
  return result;
}
const [mode, first, second, third] = process.argv.slice(2);
let result: JsonObject;
switch (mode) {
  case 'start': {
    const name = required(first, 'name');
    const source = await readFile(required(second, 'source.rs'), 'utf8');
    const expectedHash = third ?? (await command('view', { target: name })).hash;
    if (typeof expectedHash !== 'string' || !/^[a-f0-9]{64}$/.test(expectedHash)) throw new Error('Invalid expected definition hash');
    const requestId = process.env.LOOM_UPDATE_REQUEST_ID ?? crypto.randomUUID();
    console.error(`Update recovery ID: ${requestId}`);
    result = await command('update', { name, source, expected_hash: expectedHash, request_id: requestId });
    break;
  }
  case 'view':
    result = await command('update_view', { id: required(first, 'update id') });
    break;
  case 'repair': {
    // The manifest is a name -> {source, deps?, allowed_effects?} map.
    const changes = object(JSON.parse(await readFile(required(third, 'repairs.json'), 'utf8')));
    for (const [name, change] of Object.entries(changes)) {
      if (typeof object(change).source !== 'string') throw new Error(`${name}: source must be a string`);
    }
    result = await command('update_repair', { id: required(first, 'update id'), revision: revision(second), changes });
    break;
  }
  case 'rebase':
    result = await command('update_rebase', { id: required(first, 'update id'), revision: revision(second) });
    break;
  case 'abort':
    result = await command('update_abort', { id: required(first, 'update id'), revision: revision(second) });
    break;
  default:
    throw new Error('Usage: update.ts start NAME SOURCE.rs [EXPECTED_HASH] | view ID | repair ID REVISION REPAIRS.json | rebase ID REVISION | abort ID REVISION');
}
console.log(JSON.stringify(result, null, 2));
const update = object(result.update ?? result);
if (update.status === 'needs_repair') process.exitCode = 2;
else if (update.status === 'conflict') process.exitCode = 3;

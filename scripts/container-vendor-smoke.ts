import { readFile } from 'node:fs/promises';

interface Reply { ok: boolean; result: unknown; diagnostics: unknown[] }
interface Definition { def: { hash: string } }
const endpoint = process.env.LOOM_URL ?? 'http://127.0.0.1:18788';
const token = process.env.LOOM_TOKEN;
if (!token) throw new Error('LOOM_TOKEN is required');
async function operation<T>(name: string, body: unknown): Promise<T> {
  const response = await fetch(`${endpoint}/v1/${name}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error(`${name}: HTTP ${response.status}: ${await response.text()}`);
  const reply = await response.json() as Reply;
  if (!reply.ok) {
    if (typeof reply.result === 'object' && reply.result !== null && Object.keys(reply.result).length === 1 && '$ref' in reply.result && typeof reply.result.$ref === 'string') {
      const stored = await fetch(`${endpoint}/v1/cas/${reply.result.$ref}`, { headers: { Authorization: `Bearer ${token}`, Accept: 'application/json' } });
      throw new Error(`${JSON.stringify(reply)}\nResolved build details: ${await stored.text()}`);
    }
    throw new Error(JSON.stringify(reply));
  }
  return reply.result as T;
}
const source = await readFile(new URL('../examples/bundles/itoa.json', import.meta.url), 'utf8');
const definition = await operation<Definition>('define', { name: 'container-vendored-itoa', lang: 'rust', source });
const result = await operation<unknown>('command', { command: 'call', args: { hash: definition.def.hash, args: [42] } });
if (result !== '42') throw new Error(`Vendored container component returned ${JSON.stringify(result)}`);
console.log('1/1 container sandbox vendor/build/Wasmtime call passed');

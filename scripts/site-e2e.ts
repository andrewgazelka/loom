/** Production HTTP path. All linked browser data is created by a real TS guest. */
const endpoint = process.env.LOOM_URL;
const token = process.env.LOOM_TOKEN;
if (!endpoint || !token) throw new Error('Set LOOM_URL and LOOM_TOKEN for a running loomd');
type Reply = { ok: boolean; seq: number; result: any; diagnostics: unknown[] };
let passed = 0;
const headers = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
function assert(condition: unknown, message: string): asserts condition { if (!condition) throw new Error(message); }
async function operation(name: string, body: unknown): Promise<Reply> {
  const response = await fetch(`${endpoint}/v1/${name}`, { method: 'POST', headers, body: JSON.stringify(body), signal: AbortSignal.timeout(120_000) });
  assert(response.ok, `HTTP ${response.status}: ${await response.clone().text()}`);
  const reply = await response.json() as Reply;
  assert(reply.ok, JSON.stringify(reply));
  return reply;
}
async function command(name: string, args: unknown = {}): Promise<any> {
  const reply = await operation('command', { command: name, args });
  const result = reply.result;
  if (name !== 'resolve' && result && typeof result.$ref === 'string' && Object.keys(result).length === 1) {
    return (await operation('command', { command: 'resolve', args: { hash: result.$ref } })).result;
  }
  return result;
}
async function check(name: string, run: () => Promise<void>) { await run(); passed++; console.log(`PASS ${name}`); }
let definitionHash = '';
let parentCid = '';
let leafCid = '';
try {
  await check('production UI HTML and entry module are served', async () => {
    const response = await fetch(endpoint);
    assert(response.ok && response.headers.get('content-type')?.includes('text/html'), 'UI document not served');
    const html = await response.text();
    const script = html.match(/(?:src|href)="([^"\s]+\.js)"/);
    assert(script?.[1], 'UI document has no JavaScript entry');
    const asset = await fetch(new URL(script[1], endpoint));
    assert(asset.ok && (asset.headers.get('content-type')?.includes('javascript') || asset.headers.get('content-type')?.includes('ecmascript')), 'UI entry module not served');
    await asset.body?.cancel();
  });
  await check('CAS browser requires authentication', async () => {
    const response = await fetch(`${endpoint}/v1/command`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ command: 'cas.list', args: {} }) });
    assert(response.status === 401, 'unauthenticated CAS listing was accepted');
  });
  await check('TS guest creates real linked CAS values', async () => {
    const source = `import {cas} from "loom"; export function main(): unknown {
      const leaf = cas.put({label:"Site browser leaf",answer:42});
      const parent = cas.put({label:"Site browser parent",child:{$ref:leaf.$ref}});
      return {leaf:{$ref:leaf.$ref},parent:{$ref:parent.$ref}};
    }`;
    const definition = await operation('define', { name: 'site-browser-seed', lang: 'ts', source });
    definitionHash = definition.result.def.hash;
    const values = await command('call', { hash: definitionHash, args: [] });
    leafCid = values.leaf.$ref;
    parentCid = values.parent.$ref;
    assert(typeof leafCid === 'string' && typeof parentCid === 'string' && leafCid !== parentCid, 'guest did not return distinct links');
  });
  await check('filtered listing contains live parent and paginates', async () => {
    const parent = await command('cas.inspect', { hash: parentCid });
    const filtered = await command('cas.list', { kind: 'blob', q: parent.entry.hash.slice(0, 12), limit: 10 });
    assert(filtered.items.some((item: any) => item.hash === parent.entry.hash && item.codecs.some((codec: any) => codec.cid === parentCid)), 'filtered list omitted live parent CID');
    const first = await command('cas.list', { kind: 'blob', limit: 1 });
    assert(first.items.length === 1 && first.next_cursor, 'pagination cursor missing');
    const second = await command('cas.list', { kind: 'blob', limit: 1, after: first.next_cursor });
    assert(second.items.length === 1 && second.items[0].hash > first.items[0].hash, 'cursor did not advance');
  });
  await check('inspect follows a DAG link and JSON representation', async () => {
    const parent = await command('cas.inspect', { hash: parentCid });
    const link = parent.links.find((link: any) => link.path === '/child');
    assert(link?.cid === leafCid && parent.value.child.$ref === leafCid, 'inspect lost the DAG link');
    const child = await command('cas.inspect', { hash: link.cid });
    assert(child.value.answer === 42 && child.codec.name === 'dag-cbor', 'linked child is not the stored value');
    const json = await fetch(`${endpoint}/v1/cas/${encodeURIComponent(link.cid)}`, { headers: { Authorization: `Bearer ${token}`, Accept: 'application/json' } });
    assert(json.ok && (await json.json()).answer === 42, 'explicit JSON read disagrees with inspector');
  });
  await check('component inspection returns bounded raw preview', async () => {
    const definition = await command('resolve', { hash: definitionHash });
    const component = await command('cas.inspect', { hash: definition.component_hash });
    assert(component.codec.name === 'raw' && component.entry.size > 256, 'component raw registration missing');
    assert(component.value === undefined && component.hex.length <= 512 && component.truncated, 'component preview was not bounded');
    assert(JSON.stringify(component).length < 20_000, 'inspector exposed the component body');
  });
} finally { console.log(`${passed}/6 live CAS website checks pass`); }

/** Exercise the real rmcp HTTP transport and both guest tool loops. */
import { readFile } from 'node:fs/promises';
import { LoomMcpClient, object } from './mcp-client';
const endpoint = process.env.LOOM_URL ?? 'http://127.0.0.1:8787';
const token = process.env.LOOM_TOKEN;
if (!token) throw new Error('Set LOOM_TOKEN');
const client = new LoomMcpClient({endpoint, token});
let passed = 0;
const rpc = async (method: string, params: unknown): Promise<Record<string, unknown>> => object(await client.rpc(method, params));
const tool = (name: string, args: unknown) => client.callTool(name, args);
function list(value: unknown): Record<string, unknown>[] { if (!Array.isArray(value)) throw new Error('Expected list'); return value.map(item => object(item)); }
function definitionHash(value: unknown): string { const hash = object(object(value).def).hash; if (typeof hash !== 'string') throw new Error('Missing definition hash'); return hash; }
function assert(condition: unknown, message: string): asserts condition { if (!condition) throw new Error(message); }
async function check(name: string, run: () => Promise<void>) { await run(); passed++; console.log(`PASS ${name}`); }
try {
  await check('MCP initialize and discovery', async () => {
    await client.connect();
    const tools = await rpc('tools/list', {});
    const names = list(tools.tools).map(tool => String(tool.name)).sort();
    assert(JSON.stringify(names) === JSON.stringify(['crate_add', 'loom_command', 'loom_define', 'loom_eval', 'loom_resolve', 'loom_upgrade']), `unexpected shared protocol tools: ${names.join(', ')}`);
  });
  await check('MCP TS reject retry accept execute', async () => {
    const rejected = await tool('loom_define', { name: 'mcp-ts', source: 'export function main(): unknown { return fetch("https://example.com"); }' });
    assert(!rejected.ok && rejected.diagnostics.length > 0, 'TS rejection lacks diagnostics');
    const accepted = await tool('loom_define', { name: 'mcp-ts', source: 'export function main(): number { return 42; }' });
    assert(accepted.ok, JSON.stringify(accepted));
    const call = await tool('loom_command', { command: 'call', args: { hash: definitionHash(accepted.result), args: [] } });
    assert(call.ok && call.result === 42, JSON.stringify(call));
  });
  await check('MCP Rust reject retry accept execute', async () => {
    const rejected = await tool('loom_define', { lang: 'rust', name: 'mcp-rust', source: 'pub fn main() { std::fs::read("secret").unwrap(); }' });
    assert(!rejected.ok && rejected.diagnostics.length > 0, 'Rust rejection lacks diagnostics');
    const source = await readFile(new URL('../examples/rust-add/src/lib.rs', import.meta.url), 'utf8');
    const accepted = await tool('loom_define', { lang: 'rust', name: 'mcp-rust', source });
    assert(accepted.ok, JSON.stringify(accepted));
    const call = await tool('loom_command', { command: 'call', args: { hash: definitionHash(accepted.result), args: [22, 20] } });
    assert(call.ok && call.result === 42, JSON.stringify(call));
  });
  await check('MCP prompts and resources', async () => {
    const prompts = await rpc('prompts/list', {});
    assert(list(prompts.prompts).some((prompt) => prompt.name === 'loom_intro_rust'), 'Rust prompt missing');
    const prompt = await rpc('prompts/get', { name: 'loom_intro_ts' });
    assert(list(prompt.messages).length > 0, 'TS prompt empty');
    const resource = await rpc('resources/read', { uri: 'loom://def/mcp-ts' });
    assert(JSON.parse(String(list(resource.contents)[0]?.text)).lang === 'ts', 'definition resource differs');
  });
} finally { await client.close(); console.log(`${passed}/4 native MCP checks pass`); }

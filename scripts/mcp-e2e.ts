/** Exercise the real rmcp HTTP transport and both guest tool loops. */
import { readFile } from 'node:fs/promises';
const endpoint = process.env.LOOM_URL ?? 'http://127.0.0.1:8787';
const token = process.env.LOOM_TOKEN;
if (!token) throw new Error('Set LOOM_TOKEN');
let session: string | undefined;
let id = 0;
let protocolVersion: string | undefined;
let passed = 0;
async function rpc(method: string, params: unknown, notification = false): Promise<any> {
  const headers: Record<string, string> = {
    Authorization: `Bearer ${token}`,
    'Content-Type': 'application/json',
    Accept: 'application/json, text/event-stream',
  };
  if (session) headers['Mcp-Session-Id'] = session;
  if (protocolVersion) headers['MCP-Protocol-Version'] = protocolVersion;
  const requestId = ++id;
  const response = await fetch(`${endpoint}/mcp`, {
    method: 'POST', headers,
    body: JSON.stringify({ jsonrpc: '2.0', ...(notification ? {} : { id: requestId }), method, params }),
  });
  if (!response.ok) throw new Error(`MCP ${method}: HTTP ${response.status}: ${await response.text()}`);
  session = response.headers.get('mcp-session-id') ?? session;
  if (notification || response.status === 202) return;
  let message: any;
  if (response.headers.get('content-type')?.includes('text/event-stream')) {
    const reader = response.body!.getReader();
    const decoder = new TextDecoder();
    let text = '';
    while (true) {
      const item = await reader.read();
      text += decoder.decode(item.value, { stream: !item.done });
      const packets = text.split(/\r?\n\r?\n/);
      text = packets.pop()!;
      for (const packet of packets) {
        const data = packet.split(/\r?\n/).filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
        if (!data) continue;
        const candidate = JSON.parse(data);
        if (candidate.id === requestId) { message = candidate; break; }
      }
      if (message) { await reader.cancel(); break; }
      if (item.done) throw new Error(`MCP ${method} ended without response`);
    }
  } else message = await response.json();
  if (message.error) throw new Error(JSON.stringify(message.error));
  return message.result;
}
async function tool(name: string, args: unknown): Promise<any> {
  const result = await rpc('tools/call', { name, arguments: args });
  if (result.isError) throw new Error(JSON.stringify(result));
  const content = result.content.find((item: any) => item.type === 'text');
  if (!content) throw new Error('Tool result lacks text');
  return JSON.parse(content.text);
}
function assert(condition: unknown, message: string): asserts condition { if (!condition) throw new Error(message); }
async function check(name: string, run: () => Promise<void>) { await run(); passed++; console.log(`PASS ${name}`); }
try {
  await check('MCP initialize and discovery', async () => {
    const initialized = await rpc('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'loom-e2e', version: '1' } });
    assert(typeof initialized.protocolVersion === 'string', 'server did not negotiate a protocol version');
    protocolVersion = initialized.protocolVersion;
    await rpc('notifications/initialized', {}, true);
    const tools = await rpc('tools/list', {});
    assert(tools.tools.length === 4, 'expected four shared protocol tools');
  });
  await check('MCP TS reject retry accept execute', async () => {
    const rejected = await tool('loom_define', { name: 'mcp-ts', source: 'export function main(): unknown { return fetch("https://example.com"); }' });
    assert(!rejected.ok && rejected.diagnostics.length > 0, 'TS rejection lacks diagnostics');
    const accepted = await tool('loom_define', { name: 'mcp-ts', source: 'export function main(): number { return 42; }' });
    assert(accepted.ok, JSON.stringify(accepted));
    const call = await tool('loom_command', { command: 'call', args: { hash: accepted.result.def.hash, args: [] } });
    assert(call.ok && call.result === 42, JSON.stringify(call));
  });
  await check('MCP Rust reject retry accept execute', async () => {
    const rejected = await tool('loom_define', { lang: 'rust', name: 'mcp-rust', source: 'pub fn main() { std::fs::read("secret").unwrap(); }' });
    assert(!rejected.ok && rejected.diagnostics.length > 0, 'Rust rejection lacks diagnostics');
    const source = await readFile(new URL('../examples/rust-add/src/lib.rs', import.meta.url), 'utf8');
    const accepted = await tool('loom_define', { lang: 'rust', name: 'mcp-rust', source });
    assert(accepted.ok, JSON.stringify(accepted));
    const call = await tool('loom_command', { command: 'call', args: { hash: accepted.result.def.hash, args: [22, 20] } });
    assert(call.ok && call.result === 42, JSON.stringify(call));
  });
  await check('MCP prompts and resources', async () => {
    const prompts = await rpc('prompts/list', {});
    assert(prompts.prompts.some((prompt: any) => prompt.name === 'loom_intro_rust'), 'Rust prompt missing');
    const prompt = await rpc('prompts/get', { name: 'loom_intro_ts' });
    assert(prompt.messages.length > 0, 'TS prompt empty');
    const resource = await rpc('resources/read', { uri: 'loom://def/mcp-ts' });
    assert(JSON.parse(resource.contents[0].text).lang === 'ts', 'definition resource differs');
  });
} finally { console.log(`${passed}/4 native MCP checks pass`); }

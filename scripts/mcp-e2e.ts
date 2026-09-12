/** Exercise the real rmcp HTTP transport and the Rust guest tool loop. */
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
    assert(JSON.stringify(names) === JSON.stringify(['actor_behaviors', 'actor_dead_letters', 'actor_fork', 'actor_info', 'actor_lineage', 'actor_list', 'actor_members', 'actor_promote', 'actor_promote_where', 'actor_register', 'actor_restart', 'actor_run', 'actor_send', 'actor_spawn', 'actor_sql', 'actor_stop', 'actor_tree', 'actor_validate', 'actor_whereis', 'loom_add', 'loom_command', 'loom_dependents', 'loom_diff', 'loom_find', 'loom_history', 'loom_run', 'loom_update', 'loom_view']), `unexpected shared protocol tools: ${names.join(', ')}`);
  });
  await check('MCP Rust reject retry accept execute', async () => {
    const rejected = await tool('loom_add', { name: 'mcp-rust', source: 'pub fn main() { std::fs::read("secret").unwrap(); }' });
    assert(!rejected.ok && typeof object(rejected.result).error === 'string', 'Rust rejection lacks an error');
    const source = 'pub fn sum(a: i64, b: i64) -> i64 { a + b }';
    const accepted = await tool('loom_add', { name: 'mcp-rust', source });
    assert(accepted.ok, JSON.stringify(accepted));
    const call = await tool('loom_run', { target: definitionHash(accepted.result), args: [22, 20] });
    assert(call.ok && object(call.result).output === 42, JSON.stringify(call));
  });
  await check('MCP prompts and resources', async () => {
    const prompts = await rpc('prompts/list', {});
    assert(list(prompts.prompts).some((prompt) => prompt.name === 'loom_intro_rust'), 'Rust prompt missing');
    const prompt = await rpc('prompts/get', { name: 'loom_intro_rust' });
    assert(list(prompt.messages).length > 0, 'Rust prompt empty');
    const resource = await rpc('resources/read', { uri: 'loom://def/mcp-rust' });
    assert(object(object(JSON.parse(String(list(resource.contents)[0]?.text))).def).lang === 'rust', 'definition resource differs');
  });
} finally { await client.close(); console.log(`${passed}/3 native MCP checks pass`); }

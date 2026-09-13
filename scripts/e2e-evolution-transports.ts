/** Read-only parity check using an update ID from e2e-evolution.ts. */
import { readFile } from 'node:fs/promises';
import { LoomMcpClient, object } from './mcp-client';

const timeout = 30000;
async function bounded<T>(operation: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([operation, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`Timeout after ${timeout}ms`)), timeout);
    })]);
  } finally { clearTimeout(timer); }
}
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
let client: LoomMcpClient | undefined;
let passed = 0;
try {
  const endpoint = process.env.LOOM_URL;
  const id = process.argv[2];
  assert(endpoint && id, 'Set LOOM_URL and pass an existing update ID');
  const token = process.env.LOOM_TOKEN_FILE ? (await readFile(process.env.LOOM_TOKEN_FILE, 'utf8')).trim() : process.env.LOOM_TOKEN ?? '';
  const response = await fetch(`${endpoint.replace(/\/$/, '')}/v1/command`, {
    method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ command: 'update_view', args: { id } }), signal: AbortSignal.timeout(timeout),
  });
  assert(response.ok, `HTTP ${response.status}`);
  const expected = object(await response.json());
  assert(expected.ok === true, JSON.stringify(expected));
  const expectedUpdate = object(object(expected.result).update);
  const cli = Bun.spawn([process.env.LOOM_CLI ?? 'loom', '--url', endpoint, 'update_view', id], {
    env: { ...process.env, LOOM_TOKEN: token }, stdout: 'pipe', stderr: 'pipe',
  });
  try {
    const output = { stdout: '', stderr: '', status: -1 };
    await bounded(Promise.all([
      new Response(cli.stdout).text().then(value => { output.stdout = value; }),
      new Response(cli.stderr).text().then(value => { output.stderr = value; }),
      cli.exited.then(value => { output.status = value; }),
    ]));
    if (output.stderr) process.stderr.write(output.stderr);
    assert(output.status === 0, `CLI exit ${output.status}`);
    const actual = object(JSON.parse(output.stdout));
    assert(actual.ok === true && JSON.stringify(object(actual.result).update) === JSON.stringify(expectedUpdate), `CLI update mismatch: ${output.stdout}`);
  } finally { cli.kill(); }
  passed++; console.log('PASS CLI update_view parity');
  client = new LoomMcpClient({ endpoint, token });
  await bounded(client.connect());
  const discovery = object(await bounded(client.rpc('tools/list')));
  assert(Array.isArray(discovery.tools), 'Missing MCP tools');
  const toolNames = discovery.tools.map(tool => object(tool).name);
  for (const name of ['update', 'update_view', 'update_repair', 'update_rebase', 'update_abort']) {
    assert(toolNames.includes(name), `MCP missing ${name}`);
  }
  const actual = await bounded(client.callTool('update_view', { id }));
  assert(actual.ok === true && JSON.stringify(object(actual.result).update) === JSON.stringify(expectedUpdate), `MCP update mismatch: ${JSON.stringify(actual)}`);
  passed++; console.log('PASS MCP discovery and update_view parity');
} catch (error) {
  console.error(`FAIL ${String(error)}`);
} finally {
  if (client) {
    try { await bounded(client.close()); }
    catch (error) { console.error(`MCP close: ${String(error)}`); passed = Math.min(passed, 1); }
  }
  console.log(`${passed}/2 transport checks`);
}
process.exit(passed === 2 ? 0 : 1);

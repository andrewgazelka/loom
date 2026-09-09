import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const scratch = await mkdtemp(join(tmpdir(), "loom-limits-"));
let daemon: ReturnType<typeof Bun.spawn> | undefined;
let checks = 0;
function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
  checks++;
}
try {
  const tokens = join(scratch, "tokens.json");
  await writeFile(tokens, JSON.stringify([
    { token: "owner", scopes: ["read", "execute", "define", "admin"] },
    { token: "reader", scopes: ["read"] },
  ]));
  const env = { ...process.env, LOOM_BUILD_DIR: join(scratch, "build") };
  delete env.LOOM_TOKEN;
  daemon = Bun.spawn([resolve(process.env.LOOMD_BINARY ?? "target/debug/loomd"),
    "--root", process.cwd(), "--db", join(scratch, "loom.sqlite"),
    "--bind", "127.0.0.1:0", "--tokens-file", tokens], { env, stdout: "pipe", stderr: "pipe" });
  let diagnostics = "";
  let timer: ReturnType<typeof setTimeout> | undefined;
  const address = await Promise.race([
    (async () => {
      for await (const chunk of daemon!.stderr as ReadableStream<Uint8Array>) {
        diagnostics += new TextDecoder().decode(chunk);
        const match = diagnostics.match(/loomd listening on (127\.0\.0\.1:\d+)/);
        if (match) return `http://${match[1]}`;
      }
      throw new Error(`daemon failed: ${diagnostics}`);
    })(),
    new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error(`startup timeout: ${diagnostics}`)), 15000); }),
  ]).finally(() => clearTimeout(timer));
  const post = async (operation: string, body: unknown, token = "owner") => fetch(`${address}/v1/${operation}`, {
    method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${token}` }, body: JSON.stringify(body),
  });
  check((await post("command", { command: "stats", args: {} }, "wrong")).status === 401, "invalid bearer accepted");
  const stats = await post("command", { command: "stats", args: {} }, "reader");
  check(stats.ok && (await stats.json()).ok === true, "read token cannot read stats");
  for (const request of [
    { operation: "command", body: { command: "gc", args: { limit: 1 } } },
    { operation: "define", body: { name: "forbidden", source: "export function f(): number { return 1; }" } },
    { operation: "eval", body: { source: "1" } },
  ]) {
    const response = await post(request.operation, request.body, "reader");
    check(response.status === 403, `read token allowed ${request.operation}: ${response.status}`);
  }
  const megabyte = 1024 * 1024;
  check((await post("command", { command: "stats", args: { padding: "x".repeat(megabyte) } })).status === 413, "command body exceeds 1 MiB without 413");
  check((await post("eval", { source: "x".repeat(megabyte) })).status === 413, "eval body exceeds 1 MiB without 413");
  check((await post("define", { name: "large", lang: "ts", source: "x".repeat(megabyte) })).status === 413, "TS define body exceeds 1 MiB without 413");
  check((await post("define", { name: "large", lang: "rust", source: "x".repeat(16 * megabyte) })).status === 413, "Rust define body exceeds 16 MiB without 413");
  // >1 MiB Rust input must reach the language pipeline; deterministic invalid
  // source gives diagnostics without triggering a successful expensive build.
  const rust = await post("define", { name: "rust_limit_control", lang: "rust", source: `//${"x".repeat(megabyte)}\nnot valid rust` });
  const rustBody = await rust.json();
  check(rust.status === 200 && rustBody.ok === false && rustBody.diagnostics?.length > 0, "Rust >1 MiB was not admitted for structured language checking");
  const gc = await post("command", { command: "gc", args: { limit: 1001 } });
  check((await gc.json()).ok === false, "unbounded effect collection accepted");
  const backup = await post("command", { command: "backup", args: { name: "../escape" } });
  check((await backup.json()).ok === false, "backup directory traversal accepted");
  let mcpSession: string | undefined;
  let protocolVersion: string | undefined;
  let requestId = 0;
  async function rpc(method: string, params: unknown, notification = false): Promise<any> {
    const id = ++requestId;
    const headers: Record<string, string> = { authorization: "Bearer reader", "content-type": "application/json", accept: "application/json, text/event-stream" };
    if (mcpSession) headers["Mcp-Session-Id"] = mcpSession;
    if (protocolVersion) headers["MCP-Protocol-Version"] = protocolVersion;
    const response = await fetch(`${address}/mcp`, { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", ...(notification ? {} : { id }), method, params }) });
    if (!response.ok) throw new Error(`MCP ${method}: ${response.status} ${await response.text()}`);
    mcpSession = response.headers.get("mcp-session-id") ?? mcpSession;
    if (notification || response.status === 202) return;
    let message: any;
    if (response.headers.get("content-type")?.includes("text/event-stream")) {
      const reader = response.body!.getReader();
      const decoder = new TextDecoder();
      let text = "";
      while (!message) {
        const item = await reader.read();
        text += decoder.decode(item.value, { stream: !item.done });
        const packets = text.split(/\r?\n\r?\n/);
        text = packets.pop()!;
        for (const packet of packets) {
          const data = packet.split(/\r?\n/).filter(line => line.startsWith("data:")).map(line => line.slice(5).trimStart()).join("\n");
          if (!data) continue;
          const candidate = JSON.parse(data);
          if (candidate.id === id) message = candidate;
        }
        if (message) await reader.cancel();
        else if (item.done) throw new Error("MCP stream ended before response");
      }
    } else message = await response.json();
    if (message.error) throw new Error(JSON.stringify(message.error));
    return message.result;
  }
  const initialized = await rpc("initialize", { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "loom-limits", version: "1" } });
  protocolVersion = initialized.protocolVersion;
  await rpc("notifications/initialized", {}, true);
  const denied = await rpc("tools/call", { name: "loom_define", arguments: { name: "forbidden_mcp", source: "export function main(): number { return 1; }" } });
  const denial = JSON.parse(denied.content.find((item: any) => item.type === "text").text);
  check(denial.ok === false && denial.result?.code === "forbidden", "MCP read token allowed definition");
  console.log(`${checks}/${checks} HTTP/MCP auth, scopes, limits controls pass`);
} finally {
  daemon?.kill();
  if (daemon) await daemon.exited;
  await rm(scratch, { recursive: true, force: true });
}

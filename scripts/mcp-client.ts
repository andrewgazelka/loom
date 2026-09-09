/** Streamable HTTP MCP boundary; protocol failures and Loom diagnostics stay distinct. */
export type JsonObject = Record<string, unknown>;
export interface LoomResponse { ok: boolean; seq: number; result: unknown; diagnostics: unknown[] }
export interface LoomMcpOptions { endpoint: string; token: string }
export function object(value: unknown, label = 'object'): JsonObject {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`Expected ${label}`);
  return value as JsonObject;
}
export class McpTransportError extends Error {}
export class LoomMcpClient {
  private session?: string;
  private protocolVersion?: string;
  private nextId = 0;
  private readonly url: string;
  constructor(private readonly options: LoomMcpOptions) { this.url = `${options.endpoint.replace(/\/$/, '')}/mcp`; }
  private headers(): Record<string, string> {
    return { Authorization: `Bearer ${this.options.token}`, 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream', ...(this.session ? { 'Mcp-Session-Id': this.session } : {}), ...(this.protocolVersion ? { 'MCP-Protocol-Version': this.protocolVersion } : {}) };
  }
  async connect(): Promise<void> {
    const initialized = object(await this.rpc('initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'loom-client', version: '1' } }));
    if (typeof initialized.protocolVersion !== 'string') throw new McpTransportError('MCP did not negotiate protocol version');
    this.protocolVersion = initialized.protocolVersion;
    await this.rpc('notifications/initialized', {}, true);
  }
  async rpc(method: string, params: unknown = {}, notification = false): Promise<unknown> {
    const id = ++this.nextId;
    const response = await fetch(this.url, { method: 'POST', headers: this.headers(), body: JSON.stringify({ jsonrpc: '2.0', ...(!notification ? { id } : {}), method, params }) });
    if (!response.ok) throw new McpTransportError(`MCP ${method}: HTTP ${response.status}: ${await response.text()}`);
    this.session = response.headers.get('mcp-session-id') ?? this.session;
    if (notification) { await response.body?.cancel(); return undefined; }
    let message: JsonObject;
    if (response.headers.get('content-type')?.includes('text/event-stream')) {
      if (!response.body) throw new McpTransportError(`MCP ${method}: missing stream`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let pending = '';
      let found: JsonObject | undefined;
      try {
        while (!found) {
          const chunk = await reader.read();
          pending += decoder.decode(chunk.value, { stream: !chunk.done });
          const packets = pending.split(/\r?\n\r?\n/);
          pending = packets.pop() ?? '';
          if (chunk.done && pending.trim()) packets.push(pending);
          for (const packet of packets) {
            const data = packet.split(/\r?\n/).filter(line => line.startsWith('data:')).map(line => line.slice(5).replace(/^ /, '')).join('\n');
            if (!data) continue;
            const candidate = object(JSON.parse(data), 'MCP stream message');
            if (candidate.id === id) { found = candidate; break; }
          }
          if (!found && chunk.done) throw new McpTransportError(`MCP ${method}: stream ended without matching response`);
        }
        message = found;
      } finally { await reader.cancel(); reader.releaseLock(); }
    } else message = object(await response.json(), 'MCP response');
    if (message.jsonrpc !== '2.0' || message.id !== id) throw new McpTransportError(`MCP ${method}: invalid response identity`);
    if ('error' in message) throw new McpTransportError(`MCP ${method}: ${JSON.stringify(message.error)}`);
    if (!('result' in message)) throw new McpTransportError(`MCP ${method}: missing result`);
    return message.result;
  }
  async callTool(name: string, args: unknown = {}): Promise<LoomResponse> {
    const result = object(await this.rpc('tools/call', { name, arguments: args }), 'tool result');
    if (result.isError === true) throw new McpTransportError(`MCP tool ${name}: ${JSON.stringify(result)}`);
    if (!Array.isArray(result.content)) throw new McpTransportError(`MCP tool ${name}: missing content`);
    const texts = result.content.map(value => object(value, 'content block')).filter(value => value.type === 'text');
    if (texts.length !== 1 || typeof texts[0]?.text !== 'string') throw new McpTransportError(`MCP tool ${name}: expected one JSON text block`);
    const reply = object(JSON.parse(texts[0].text), 'Loom response');
    if (typeof reply.ok !== 'boolean' || !Number.isSafeInteger(reply.seq) || !Array.isArray(reply.diagnostics) || !('result' in reply)) throw new McpTransportError(`MCP tool ${name}: invalid Loom response`);
    return { ok: reply.ok, seq: reply.seq as number, result: reply.result, diagnostics: reply.diagnostics };
  }
  async close(): Promise<void> {
    if (!this.session) return;
    const response = await fetch(this.url, { method: 'DELETE', headers: this.headers() });
    await response.body?.cancel();
    this.session = undefined;
    if (!response.ok && response.status !== 404 && response.status !== 405) throw new McpTransportError(`MCP session close: HTTP ${response.status}`);
  }
}

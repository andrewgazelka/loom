export interface Diagnostic {
  lang: string;
  file: string;
  line: number;
  col: number;
  code: string;
  message: string;
  snippet?: string;
  hint?: string;
}
export interface Reply {
  ok: boolean;
  seq: number;
  result?: unknown;
  diagnostics?: Diagnostic[];
  error?: unknown;
}
export interface Definition {
  hash: string;
  lang: string;
  name_hint?: string;
  name?: string;
  component_hash?: string;
  component_size?: number;
}
export interface Actor {
  id: string;
  behavior_hash: string;
  lang: string;
  last_seq?: number;
  spawn_ms?: number;
}
export interface LogEvent {
  seq: number;
  actor: string;
  event_hash?: string;
  event?: unknown;
  ts?: number;
}
export function format(value: unknown): string {
  return typeof value === "string"
    ? value
    : (JSON.stringify(value, null, 2) ?? "null");
}
export function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
export function items<T>(value: unknown, key: string): T[] {
  if (Array.isArray(value)) return value as T[];
  const entry = record(value)[key];
  return Array.isArray(entry) ? (entry as T[]) : [];
}
export class Client {
  constructor(
    public endpoint: string,
    public token: string,
  ) {}
  async text(hash: string): Promise<string> {
    const response = await fetch(
      `${this.endpoint.replace(/\/$/, "")}/v1/cas/${encodeURIComponent(hash)}`,
      {
        headers: this.token ? { Authorization: `Bearer ${this.token}` } : {},
      },
    );
    if (!response.ok)
      throw new Error(`Could not read content: HTTP ${response.status}`);
    return response.text();
  }
  async request(path: string, body?: unknown): Promise<Reply> {
    const response = await fetch(
      `${this.endpoint.replace(/\/$/, "")}/v1/${path}`,
      {
        method: body === undefined ? "GET" : "POST",
        headers: {
          ...(body === undefined ? {} : { "Content-Type": "application/json" }),
          ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      },
    );
    const text = await response.text();
    let data: unknown;
    try {
      data = JSON.parse(text);
    } catch {
      throw new Error(`HTTP ${response.status}: ${text.slice(0, 240)}`);
    }
    if (!response.ok)
      throw new Error(`HTTP ${response.status}: ${format(data)}`);
    const envelope = record(data);
    const reply =
      typeof envelope.ok === "boolean"
        ? (data as Reply)
        : { ok: true, seq: 0, result: data };
    const reference = record(reply.result);
    if (
      reply.ok &&
      typeof reference.$ref === "string" &&
      typeof reference.size === "number"
    ) {
      const resolved = await fetch(
        `${this.endpoint.replace(/\/$/, "")}/v1/cas/${encodeURIComponent(reference.$ref)}`,
        {
          headers: this.token ? { Authorization: `Bearer ${this.token}` } : {},
        },
      );
      if (!resolved.ok)
        throw new Error(`Could not resolve result: HTTP ${resolved.status}`);
      reply.result = await resolved.json();
    }
    return reply;
  }
  command(command: string, args: Record<string, unknown> = {}): Promise<Reply> {
    return this.request("command", { command, args });
  }
}

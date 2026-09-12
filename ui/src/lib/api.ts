export interface Diagnostic {
  lang: "rust";
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
  sig?: unknown;
  allowed_effects?: string[] | null;
  observed_effects?: string[];
  hash: string;
  lang: "rust";
  name_hint?: string;
  name?: string;
  component_hash?: string | null;
  component_size?: number | null;
}
export interface Actor {
  id: string;
  behavior_hash: string;
  lang: "rust";
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
export class AuthenticationError extends Error {
  constructor() { super("Connection rejected. Enter a valid bearer token in Connection settings, then connect again."); this.name = "AuthenticationError"; }
}
export class Client {
  private controller = new AbortController();
  private rejected = false;
  dispose() { this.controller.abort(); }
  private async fetch(input: string, init: RequestInit = {}): Promise<Response> {
    if (this.rejected) throw new AuthenticationError();
    const response = await fetch(input, {...init, signal:this.controller.signal});
    if (response.status === 401) {
      const error = new AuthenticationError();
      if (!this.controller.signal.aborted && !this.rejected) { this.rejected = true; this.onUnauthorized?.(error); }
      throw error;
    }
    return response;
  }

  constructor(
    public endpoint: string,
    public token: string,
    private onUnauthorized?: (error: AuthenticationError) => void,
  ) {}
  async bytes(hash: string, limit = 262144, accept = "application/octet-stream"): Promise<Uint8Array> {
    const response = await this.fetch(`${this.endpoint.replace(/\/$/, "")}/v1/cas/${encodeURIComponent(hash)}`, {headers:{Accept:accept, ...(this.token ? {Authorization:`Bearer ${this.token}`} : {})}});
    if (!response.ok) throw new Error(`Could not read content: HTTP ${response.status}`);
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Content body unavailable");
    const chunks: Uint8Array[] = [];
    let size = 0;
    try { for (;;) {
      const next = await reader.read();
      if(next.done) break;
      size += next.value.length;
      if(size > limit) { await reader.cancel(); throw new Error(`Content exceeds the ${limit / 1024} KiB diff preview limit`); }
      chunks.push(next.value);
    }} finally { reader.releaseLock(); }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for(const chunk of chunks) { bytes.set(chunk,offset); offset += chunk.length; }
    return bytes;
  }
  async text(hash: string): Promise<string> {
    const response = await this.fetch(
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
    const response = await this.fetch(
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
      Object.keys(reference).length === 1
    ) {
      const resolved = await this.fetch(
        `${this.endpoint.replace(/\/$/, "")}/v1/cas/${encodeURIComponent(reference.$ref)}`,
        {
          headers: {
            Accept: "application/json",
            ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
          },
        },
      );
      if (resolved.status === 406) return reply;
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

export interface CasCodec {
  code: number;
  name: string;
  cid: string;
}
export interface CasEntry {
  hash: string;
  kind: string;
  size: number;
  created_at: number;
  codecs: CasCodec[];
}
export interface CasListing {
  items: CasEntry[];
  next_cursor: string | null;
}
export interface CasLink {
  path: string;
  cid: string;
}
export interface CasInspection {
  entry: CasEntry;
  codec: CasCodec;
  value?: unknown;
  links: CasLink[];
  text?: string;
  hex: string;
  truncated: boolean;
}
export function resultOf(reply: Reply): unknown {
  if (!reply.ok)
    throw new Error(format(reply.result ?? reply.error ?? reply.diagnostics));
  return reply.result;
}
function casEntry(value: unknown): CasEntry {
  const entry = record(value);
  if (
    typeof entry.hash !== "string" ||
    typeof entry.kind !== "string" ||
    typeof entry.size !== "number" ||
    typeof entry.created_at !== "number" ||
    !Array.isArray(entry.codecs)
  )
    throw new Error("Invalid CAS entry from server");
  return {
    ...entry,
    codecs: entry.codecs.map(casCodec),
  } as unknown as CasEntry;
}
function casCodec(value: unknown): CasCodec {
  const codec = record(value);
  if (
    typeof codec.code !== "number" ||
    typeof codec.name !== "string" ||
    typeof codec.cid !== "string"
  )
    throw new Error("Invalid CAS codec from server");
  return codec as unknown as CasCodec;
}
export function casListing(reply: Reply): CasListing {
  const value = record(resultOf(reply));
  if (
    !Array.isArray(value.items) ||
    (value.next_cursor !== null && typeof value.next_cursor !== "string")
  )
    throw new Error("Invalid CAS listing from server");
  return { items: value.items.map(casEntry), next_cursor: value.next_cursor };
}
export function casInspection(reply: Reply): CasInspection {
  const value = record(resultOf(reply));
  if (
    !Array.isArray(value.links) ||
    typeof value.hex !== "string" ||
    typeof value.truncated !== "boolean"
  )
    throw new Error("Invalid CAS inspection from server");
  const links = value.links.map((link) => {
    const row = record(link);
    if (typeof row.path !== "string" || typeof row.cid !== "string")
      throw new Error("Invalid CAS link from server");
    return { path: row.path, cid: row.cid };
  });
  return {
    entry: casEntry(value.entry),
    codec: casCodec(value.codec),
    links,
    hex: value.hex,
    truncated: value.truncated,
    ...("value" in value ? { value: value.value } : {}),
    ...(typeof value.text === "string" ? { text: value.text } : {}),
  };
}
export function short(value: string, length = 16): string {
  return value.length > length + 4
    ? `${value.slice(0, length)}…${value.slice(-4)}`
    : value;
}
export function bytes(size: number): string {
  return size < 1024
    ? `${size} B`
    : size < 1024 * 1024
      ? `${(size / 1024).toFixed(1)} KB`
      : `${(size / 1024 / 1024).toFixed(1)} MB`;
}
export function cid(value: string): boolean {
  return /^b[a-z2-7]{30,}$/.test(value) || /^[a-f0-9]{64}$/.test(value);
}

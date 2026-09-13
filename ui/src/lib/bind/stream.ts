import { readTree, type Tree } from "./patch";

export type SortValue = null | boolean | number | string | SortValue[];
export interface TreeRow { key: string; sort: SortValue; tree: Tree }
export interface ChangedRow {
  change_id: number;
  change_type: -1 | 0 | 1;
  table: string;
  id: number;
  before: TreeRow | null;
  after: TreeRow | null;
}
export interface FrameContext { source: string; seq: number; key: string }
export interface Snapshot extends FrameContext { type: "snapshot"; change_id: number; rows: TreeRow[] }
export interface Delta extends FrameContext { type: "delta"; cause: string | null; rows: ChangedRow[] }
export interface Resnapshot { type: "resnapshot"; source: string }
export interface DeadLetter { type: "dead_letter"; key: string; error: string; source?: string }
export type Frame = Snapshot | Delta | Resnapshot | DeadLetter;
export interface DeltaStream { subscribe(receive: (frame: Frame) => void): () => void }

function object(value: unknown, name: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${name}: expected object`);
  return value as Record<string, unknown>;
}
function string(value: unknown, name: string): string {
  if (typeof value !== "string") throw new Error(`${name}: expected string`);
  return value;
}
function integer(value: unknown, name: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) throw new Error(`${name}: expected safe integer`);
  return value;
}
function array(value: unknown, name: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`${name}: expected array`);
  return value;
}
function blob(value: unknown, name: string): unknown {
  const bytes = array(value, name);
  if (!bytes.every((byte) => typeof byte === "number" && Number.isInteger(byte) && byte >= 0 && byte <= 255))
    throw new Error(`${name}: expected UTF-8 JSON bytes`);
  try { return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(bytes as number[]))); }
  catch (error) { throw new Error(`${name}: invalid UTF-8 JSON: ${error}`); }
}
function sort(value: unknown): SortValue {
  if (value === null || typeof value === "boolean" || typeof value === "string") return value;
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (Array.isArray(value)) return value.map(sort);
  throw new Error("tree.sort: expected scalar or array of scalars");
}
function treeRow(value: unknown): TreeRow {
  const row = object(value, "tree row");
  const key = string(row.key, "tree.key");
  return { key, sort: sort(blob(row.sort, `tree ${key}.sort`)), tree: readTree(blob(row.tree, `tree ${key}.tree`)) };
}
function context(row: Record<string, unknown>): FrameContext {
  const result: FrameContext = {
    source: string(row.source, "frame.source"), seq: integer(row.seq, "frame.seq"), key: string(row.key, "frame.key"),
  };
  return result;
}

/** This is the sole translation boundary from SQL blob bytes into browser trees. */
export function parseFrame(value: unknown): Frame {
  const row = object(value, "frame");
  if (row.type === "resnapshot") return { type: "resnapshot", source: string(row.source, "frame.source") };
  if (row.type === "dead_letter") {
    const frame: DeadLetter = { type: "dead_letter", key: string(row.key, "frame.key"), error: string(row.error, "frame.error") };
    if (row.source !== undefined) frame.source = string(row.source, "frame.source");
    return frame;
  }
  if (row.type === "snapshot") {
    if (row.table !== "tree") throw new Error(`snapshot.table: expected tree, received ${row.table}`);
    return {
      type: "snapshot", ...context(row), change_id: integer(row.change_id, "snapshot.change_id"),
      rows: array(row.rows, "snapshot.rows").map((value) => {
        const item = object(value, "snapshot row");
        integer(item.id, "snapshot row.id");
        return treeRow(item.after);
      }),
    };
  }
  if (row.type !== "delta") throw new Error(`frame.type: unsupported flag ${row.type}`);
  return {
    type: "delta", ...context(row), cause: row.cause === null ? null : string(row.cause, "frame.cause"),
    rows: array(row.rows, "delta.rows").map((value) => {
      const item = object(value, "delta row");
      const operation = item.change_type;
      if (operation !== -1 && operation !== 0 && operation !== 1)
        throw new Error(`delta.change_type: unsupported flag ${operation}`);
      if (item.table !== "tree") throw new Error(`delta.table: expected tree, received ${item.table}`);
      const before = item.before === null ? null : treeRow(item.before);
      const after = item.after === null ? null : treeRow(item.after);
      if ((operation === -1 && (!before || after)) || (operation !== -1 && !after))
        throw new Error(`delta.change_type ${operation}: missing or contradictory row image`);
      if (before && after && before.key !== after.key) throw new Error("delta: a live tree key cannot change");
      return {
        change_id: integer(item.change_id, "delta.change_id"), change_type: operation, table: "tree",
        id: integer(item.id, "delta.id"), before, after,
      };
    }),
  };
}

// Cap stays opaque: JSON.parse would round its u64 cap_id and epoch in JavaScript.
export interface Subscription { actor: string; table: "tree"; cap: string }
export interface SocketOptions {
  endpoint: string;
  token: string;
  subscription: Subscription;
  onError: (error: Error) => void;
}

/** One socket owns one host subscriber; close() removes its server rows. */
export class SocketStream implements DeltaStream {
  private socket: WebSocket;
  private receivers = new Set<(frame: Frame) => void>();
  private authenticated = false;
  private closed = false;
  constructor(options: SocketOptions) {
    const url = new URL(options.endpoint || location.origin);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash)
      throw new Error("stream endpoint: expected HTTP(S) URL without credentials, query or fragment");
    url.pathname = `${url.pathname.replace(/\/$/, "")}/v1/stream`;
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    this.socket = new WebSocket(url);
    this.socket.onopen = () => this.socket.send(JSON.stringify({ token: options.token, after: 0 }));
    this.socket.onmessage = (event) => {
      try {
        const value: unknown = JSON.parse(String(event.data));
        const row = object(value, "stream");
        if (!this.authenticated) {
          if (row.ok !== true) throw new Error("stream authentication rejected");
          this.authenticated = true;
          this.socket.send(JSON.stringify({ subscribe: options.subscription }));
          return;
        }
        if (row.error !== undefined && row.type !== "dead_letter") throw new Error(string(row.error, "stream.error"));
        // The same endpoint also carries definition events and subscription acknowledgements.
        if (row.type === undefined && (row.ok === true || (Number.isSafeInteger(row.seq) && typeof row.ts === "number" && "event" in row))) return;
        const frame = parseFrame(value);
        if ("source" in frame && frame.source !== undefined && frame.source !== options.subscription.actor)
          throw new Error(`stream source ${frame.source}: expected ${options.subscription.actor}`);
        for (const receive of this.receivers) receive(frame);
      } catch (error) {
        options.onError(error instanceof Error ? error : new Error(String(error)));
        this.close();
      }
    };
    this.socket.onerror = () => options.onError(new Error(`actor ${options.subscription.actor}: stream transport failed`));
    this.socket.onclose = () => {
      if (!this.closed) options.onError(new Error(`actor ${options.subscription.actor}: stream closed; reconnect for a fresh snapshot`));
      this.closed = true;
      this.receivers.clear();
    };
  }
  subscribe(receive: (frame: Frame) => void): () => void {
    if (this.closed) throw new Error("stream is closed");
    this.receivers.add(receive);
    return () => { this.receivers.delete(receive); };
  }
  close() {
    this.closed = true;
    this.receivers.clear();
    this.socket.close();
  }
}

/** Snapshot `/v1/events` by pages, then follow `/v1/stream`; reconnect with capped backoff and re-snapshot from the last seq. */
import { parseJournalEvent, type JournalEvent } from "./feed";

export type Fetcher = (input: string, init?: RequestInit) => Promise<Response>;
/** The subset of `WebSocket` the client drives; tests inject a fake. */
export interface SocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
}
export type SocketFactory = (url: string) => SocketLike;
export type Schedule = (run: () => void, delayMs: number) => () => void;

export type ConnectionState =
  | { kind: "idle" }
  | { kind: "snapshot"; after: number }
  | { kind: "connecting"; after: number }
  | { kind: "live"; after: number }
  | { kind: "reconnecting"; delayMs: number; attempt: number }
  | { kind: "stopped" };

export interface BoardClientOptions {
  /** HTTP(S) origin, or empty for the page origin. */
  endpoint: string;
  token: string;
  onEvents: (events: JournalEvent[]) => void;
  onState: (state: ConnectionState) => void;
  onError: (message: string) => void;
  fetch?: Fetcher;
  socket?: SocketFactory;
  schedule?: Schedule;
  /** Page size for the snapshot; the daemon caps at 1000. */
  pageSize?: number;
  origin?: string;
}

export const BACKOFF_BASE_MS = 1000;
export const BACKOFF_CAP_MS = 10_000;
export function backoffDelay(attempt: number): number {
  return Math.min(BACKOFF_BASE_MS * 2 ** attempt, BACKOFF_CAP_MS);
}

/** `/v1/stream` on the endpoint or the page origin, over ws(s). */
export function streamUrl(endpoint: string, origin: string): string {
  const url = new URL(endpoint || origin);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  )
    throw new Error(
      "stream endpoint: expected HTTP(S) URL without credentials, query or fragment",
    );
  url.pathname = `${url.pathname.replace(/\/$/, "")}/v1/stream`;
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

const defaultSchedule: Schedule = (run, delayMs) => {
  const handle = setTimeout(run, delayMs);
  return () => clearTimeout(handle);
};

export class BoardClient {
  private readonly base: string;
  private readonly fetcher: Fetcher;
  private readonly socketFactory: SocketFactory;
  private readonly schedule: Schedule;
  private readonly pageSize: number;
  private readonly origin: string;
  private socket: SocketLike | undefined;
  private cancelTimer: (() => void) | undefined;
  private stopped = false;
  private attempt = 0;
  /** Last seq folded in; the next snapshot and the stream subscription start after it. */
  after = 0;
  state: ConnectionState = { kind: "idle" };

  constructor(private readonly options: BoardClientOptions) {
    this.base = options.endpoint.trim().replace(/\/$/, "");
    this.fetcher = options.fetch ?? ((input, init) => fetch(input, init));
    this.socketFactory =
      options.socket ?? ((url) => new WebSocket(url) as unknown as SocketLike);
    this.schedule = options.schedule ?? defaultSchedule;
    this.pageSize = options.pageSize ?? 1000;
    this.origin =
      options.origin ??
      (typeof location === "undefined" ? "http://127.0.0.1" : location.origin);
  }

  private headers(json = false): Record<string, string> {
    return {
      ...(json ? { "Content-Type": "application/json" } : {}),
      ...(this.options.token ? { Authorization: `Bearer ${this.options.token}` } : {}),
    };
  }

  private setState(state: ConnectionState) {
    this.state = state;
    this.options.onState(state);
  }

  /** Unwrap the `{ok, result}` envelope of one HTTP reply. */
  private async envelope(response: Response, what: string): Promise<unknown> {
    const body = await response.text();
    if (!response.ok)
      throw new Error(`${what}: HTTP ${response.status}: ${body.slice(0, 400)}`);
    let parsed: unknown;
    try {
      parsed = JSON.parse(body);
    } catch {
      throw new Error(`${what}: HTTP ${response.status} returned invalid JSON`);
    }
    const envelope = record(parsed);
    if (envelope.ok !== true) {
      const failure = record(envelope.result);
      throw new Error(
        `${what}: ${typeof failure.error === "string" ? failure.error : JSON.stringify(envelope.result ?? parsed)}`,
      );
    }
    return envelope.result;
  }

  /** `POST /v1/command`. */
  async command(command: string, args: Record<string, unknown> = {}): Promise<unknown> {
    const response = await this.fetcher(`${this.base}/v1/command`, {
      method: "POST",
      headers: this.headers(true),
      body: JSON.stringify({ command, args }),
    });
    return this.envelope(response, command);
  }

  /** `GET /v1/cas/<hash>` as text. */
  async text(hash: string): Promise<string> {
    if (!/^[a-f0-9]{64}$/.test(hash)) throw new Error(`cas: invalid hash ${hash}`);
    const response = await this.fetcher(`${this.base}/v1/cas/${hash}`, {
      headers: this.headers(),
    });
    if (!response.ok) throw new Error(`cas ${hash.slice(0, 8)}: HTTP ${response.status}`);
    return response.text();
  }

  /** Page `/v1/events` from `after` until a page is shorter than the page size. */
  async snapshot(): Promise<void> {
    for (;;) {
      if (this.stopped) return;
      this.setState({ kind: "snapshot", after: this.after });
      const response = await this.fetcher(
        `${this.base}/v1/events?after=${this.after}&limit=${this.pageSize}`,
        { headers: this.headers() },
      );
      const result = await this.envelope(response, "events");
      if (!Array.isArray(result)) throw new Error("events: result is not an array");
      const events = result.map(parseJournalEvent);
      if (events.length) {
        const last = events[events.length - 1]!;
        this.after = Math.max(this.after, last.seq);
        this.options.onEvents(events);
      }
      if (events.length < this.pageSize) return;
    }
  }

  start(): void {
    this.stopped = false;
    void this.cycle();
  }

  stop(): void {
    this.stopped = true;
    this.cancelTimer?.();
    this.cancelTimer = undefined;
    const socket = this.socket;
    this.socket = undefined;
    socket?.close();
    this.setState({ kind: "stopped" });
  }

  private async cycle(): Promise<void> {
    try {
      await this.snapshot();
    } catch (error) {
      if (this.stopped) return;
      this.options.onError(error instanceof Error ? error.message : String(error));
      this.reconnect();
      return;
    }
    if (this.stopped) return;
    this.open();
  }

  private open(): void {
    let socket: SocketLike;
    try {
      socket = this.socketFactory(streamUrl(this.base, this.origin));
    } catch (error) {
      this.options.onError(error instanceof Error ? error.message : String(error));
      this.reconnect();
      return;
    }
    this.socket = socket;
    this.setState({ kind: "connecting", after: this.after });
    let authenticated = false;
    socket.onopen = () => {
      socket.send(JSON.stringify({ token: this.options.token, after: this.after }));
    };
    socket.onmessage = (message) => {
      if (this.socket !== socket) return;
      let value: unknown;
      try {
        value = JSON.parse(String(message.data));
      } catch {
        this.options.onError("stream: frame is not JSON");
        return;
      }
      const row = record(value);
      if (typeof row.error === "string" && row.type !== "dead_letter") {
        this.options.onError(`stream: ${row.error}`);
        return;
      }
      if (!authenticated) {
        if (row.ok === true && !("seq" in row)) {
          authenticated = true;
          this.attempt = 0;
          this.setState({ kind: "live", after: this.after });
        }
        return;
      }
      if (!Number.isSafeInteger(row.seq) || !("event" in row)) return; // actor table frames
      try {
        const event = parseJournalEvent(row);
        if (event.seq <= this.after) return;
        this.after = event.seq;
        this.options.onEvents([event]);
      } catch (error) {
        this.options.onError(error instanceof Error ? error.message : String(error));
      }
    };
    socket.onerror = () => {
      // The close event that follows carries the reconnect.
    };
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = undefined;
      if (this.stopped) return;
      if (!authenticated)
        this.options.onError(
          "stream closed before the subscription was accepted; check the token and that the daemon allows read scope",
        );
      this.reconnect();
    };
  }

  private reconnect(): void {
    if (this.stopped) return;
    const delayMs = backoffDelay(this.attempt);
    this.attempt += 1;
    this.setState({ kind: "reconnecting", delayMs, attempt: this.attempt });
    this.cancelTimer?.();
    this.cancelTimer = this.schedule(() => {
      this.cancelTimer = undefined;
      void this.cycle();
    }, delayMs);
  }
}

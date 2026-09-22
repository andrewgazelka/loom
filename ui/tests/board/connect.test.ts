import { describe, expect, test } from "bun:test";
import {
  BoardClient,
  backoffDelay,
  streamUrl,
  type ConnectionState,
  type SocketLike,
} from "../../src/lib/board/connect";
import type { JournalEvent } from "../../src/lib/board/feed";

class FakeSocket implements SocketLike {
  sent: string[] = [];
  closed = false;
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  constructor(public url: string) {}
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.closed = true;
  }
  open() {
    this.onopen?.({});
  }
  receive(value: unknown) {
    this.onmessage?.({ data: typeof value === "string" ? value : JSON.stringify(value) });
  }
  drop() {
    this.onclose?.({});
  }
}

function row(seq: number): { seq: number; ts: number; event: { type: string } } {
  return { seq, ts: 1_758_400_000 + seq, event: { type: "effect_recorded" } };
}

/** A daemon with `total` events, served by `/v1/events` pages and a recording `/v1/command`. */
function harness(total: number, pageSize: number) {
  const requests: string[] = [];
  const commands: unknown[] = [];
  const sockets: FakeSocket[] = [];
  const events: JournalEvent[] = [];
  const states: ConnectionState[] = [];
  const errors: string[] = [];
  const timers: { delay: number; run: () => void }[] = [];
  const fetcher = async (input: string, init?: RequestInit): Promise<Response> => {
    requests.push(input);
    const url = new URL(input, "http://daemon.test");
    if (url.pathname === "/v1/events") {
      const after = Number(url.searchParams.get("after"));
      const limit = Number(url.searchParams.get("limit"));
      const page = [];
      for (let seq = after + 1; seq <= total && page.length < limit; seq++) page.push(row(seq));
      return new Response(JSON.stringify({ ok: true, seq: total, result: page, diagnostics: [] }), {
        headers: { "Content-Type": "application/json" },
      });
    }
    if (url.pathname === "/v1/command") {
      commands.push(JSON.parse(String(init?.body)));
      return new Response(
        JSON.stringify({ ok: true, seq: total, result: { id: "root", status: "running", behavior_hash: "", cursor: 0, children: [] } }),
      );
    }
    return new Response("missing", { status: 404 });
  };
  const client = new BoardClient({
    endpoint: "http://daemon.test",
    token: "secret-token",
    pageSize,
    fetch: fetcher,
    socket: (url) => {
      const socket = new FakeSocket(url);
      sockets.push(socket);
      return socket;
    },
    schedule: (run, delay) => {
      const timer = { delay, run };
      timers.push(timer);
      return () => {
        const index = timers.indexOf(timer);
        if (index >= 0) timers.splice(index, 1);
      };
    },
    onEvents: (batch) => events.push(...batch),
    onState: (state) => states.push(state),
    onError: (message) => errors.push(message),
  });
  return { client, requests, commands, sockets, events, states, errors, timers };
}

/** Let a chain of fetch pages and `Response.text()` reads complete (macrotask turns, not microticks). */
async function settle() {
  for (let i = 0; i < 6; i++) await new Promise((resolve) => setTimeout(resolve, 0));
}

describe("board connection", () => {
  test("snapshot pages until a short page, then subscribes after the last seq with the bearer token", async () => {
    const h = harness(5, 2);
    h.client.start();
    await settle();
    expect(h.requests).toEqual([
      "http://daemon.test/v1/events?after=0&limit=2",
      "http://daemon.test/v1/events?after=2&limit=2",
      "http://daemon.test/v1/events?after=4&limit=2",
    ]);
    expect(h.events.map((item) => item.seq)).toEqual([1, 2, 3, 4, 5]);
    expect(h.sockets).toHaveLength(1);
    const socket = h.sockets[0]!;
    expect(socket.url).toBe("ws://daemon.test/v1/stream");
    expect(socket.sent).toEqual([]);
    socket.open();
    expect(socket.sent).toEqual([JSON.stringify({ token: "secret-token", after: 5 })]);
    expect(h.states.at(-1)).toEqual({ kind: "connecting", after: 5 });
    socket.receive({ ok: true });
    expect(h.states.at(-1)).toEqual({ kind: "live", after: 5 });
    expect(h.errors).toEqual([]);
  });

  test("an exact-size final page needs one more empty page; an empty journal subscribes after 0", async () => {
    const exact = harness(4, 2);
    exact.client.start();
    await settle();
    expect(exact.requests).toHaveLength(3);
    expect(exact.sockets).toHaveLength(1);
    const none = harness(0, 1000);
    none.client.start();
    await settle();
    expect(none.requests).toEqual(["http://daemon.test/v1/events?after=0&limit=1000"]);
    none.sockets[0]!.open();
    expect(none.sockets[0]!.sent).toEqual([JSON.stringify({ token: "secret-token", after: 0 })]);
  });

  test("stream frames with seq are events, frames without seq are ignored, errors are surfaced", async () => {
    const h = harness(1, 1000);
    h.client.start();
    await settle();
    const socket = h.sockets[0]!;
    socket.open();
    socket.receive({ ok: true });
    socket.receive(row(2));
    socket.receive({ type: "snapshot", source: "view-1", rows: [] });
    socket.receive({ ok: true });
    socket.receive(row(2));
    socket.receive(row(3));
    socket.receive({ error: "actor <none> seq -1: subscribe: actor node is not configured" });
    socket.receive("not json");
    expect(h.events.map((item) => item.seq)).toEqual([1, 2, 3]);
    expect(h.client.after).toBe(3);
    expect(h.errors).toEqual([
      "stream: actor <none> seq -1: subscribe: actor node is not configured",
      "stream: frame is not JSON",
    ]);
  });

  test("a closed socket reconnects with 1, 2, 4, 8, 10 second backoff and re-snapshots from the last seq", async () => {
    const h = harness(3, 1000);
    h.client.start();
    await settle();
    const delays: number[] = [];
    for (let attempt = 0; attempt < 5; attempt++) {
      const socket = h.sockets.at(-1)!;
      socket.open();
      socket.drop();
      expect(h.timers).toHaveLength(1);
      const timer = h.timers.pop()!;
      delays.push(timer.delay);
      expect(h.states.at(-1)).toEqual({ kind: "reconnecting", delayMs: timer.delay, attempt: attempt + 1 });
      timer.run();
      await settle();
    }
    expect(delays).toEqual([1000, 2000, 4000, 8000, 10000]);
    expect([0, 1, 2, 3, 4, 5].map(backoffDelay)).toEqual([1000, 2000, 4000, 8000, 10000, 10000]);
    expect(h.requests.filter((url) => url.includes("/v1/events"))).toEqual(
      Array(6).fill("http://daemon.test/v1/events?after=3&limit=1000").map((url, index) =>
        index === 0 ? "http://daemon.test/v1/events?after=0&limit=1000" : url,
      ),
    );
    expect(h.events.map((item) => item.seq)).toEqual([1, 2, 3]);
    expect(h.errors.every((message) => message.startsWith("stream closed before"))).toBe(true);
    // A successful subscription resets the backoff.
    const socket = h.sockets.at(-1)!;
    socket.open();
    socket.receive({ ok: true });
    socket.drop();
    expect(h.timers.at(-1)?.delay).toBe(1000);
  });

  test("stop closes the socket and cancels a pending reconnect", async () => {
    const h = harness(1, 1000);
    h.client.start();
    await settle();
    const socket = h.sockets[0]!;
    socket.open();
    socket.drop();
    expect(h.timers).toHaveLength(1);
    h.client.stop();
    expect(h.timers).toHaveLength(0);
    expect(h.states.at(-1)).toEqual({ kind: "stopped" });
    h.timers.forEach((timer) => timer.run());
    await settle();
    expect(h.sockets).toHaveLength(1);
  });

  test("a failing snapshot is reported and retried with backoff", async () => {
    const h = harness(1, 1000);
    const failing = new BoardClient({
      endpoint: "http://daemon.test",
      token: "t",
      fetch: async () => new Response("unauthorized", { status: 401 }),
      socket: () => {
        throw new Error("no socket expected");
      },
      schedule: (run, delay) => {
        h.timers.push({ delay, run });
        return () => {};
      },
      onEvents: () => {},
      onState: (state) => h.states.push(state),
      onError: (message) => h.errors.push(message),
    });
    failing.start();
    await settle();
    expect(h.errors).toEqual(["events: HTTP 401: unauthorized"]);
    expect(h.timers.map((timer) => timer.delay)).toEqual([1000]);
  });

  test("command unwraps the envelope and fails on ok:false with the server's error", async () => {
    const h = harness(0, 1000);
    expect(await h.client.command("tree", {})).toMatchObject({ id: "root" });
    expect(h.commands).toEqual([{ command: "tree", args: {} }]);
    const refusing = new BoardClient({
      endpoint: "",
      origin: "http://page.test",
      token: "t",
      fetch: async () =>
        new Response(JSON.stringify({ ok: false, seq: 1, result: { error: "actor x seq -1: unknown" } })),
      onEvents: () => {},
      onState: () => {},
      onError: () => {},
    });
    await expect(refusing.command("info", { id: "x" })).rejects.toThrow("info: actor x seq -1: unknown");
    expect(streamUrl("", "http://page.test")).toBe("ws://page.test/v1/stream");
    expect(streamUrl("https://daemon.example/base/", "http://page.test")).toBe(
      "wss://daemon.example/base/v1/stream",
    );
    expect(() => streamUrl("http://user:pw@daemon.example", "http://page.test")).toThrow("credentials");
  });
});

import { describe, expect, test } from "bun:test";
import { get } from "svelte/store";
import { commandById, V } from "../src/lib/workbench/commands";
import { Workspace, panelKey } from "../src/lib/workbench/workspace";
import { WorkbenchClient, HttpTransport } from "../src/lib/workbench/client";
import { Journal } from "../src/lib/workbench/journal";

class MemoryStorage {
  values = new Map<string, string>();
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}
describe("workspace ownership", () => {
  test("migrates aggregate draft storage without losing source", () => {
    const storage = new MemoryStorage();
    const command = commandById(V.update);
    const identity = panelKey(command, { name: "first" });
    const source = "pub fn unfinished() {";
    storage.setItem(
      "loom.drafts.v1:http://one",
      JSON.stringify({ [identity]: { name: "first", source } }),
    );
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    expect(get(workspace.storageError)).toBe("");
    expect(get(workspace.open(command, { name: "first" })).values.source).toBe(
      source,
    );
    expect(JSON.parse(storage.getItem("loom.drafts.v1:http://one")!)).toEqual([
      identity,
    ]);
    const reloaded = new Workspace();
    reloaded.connect("http://one", storage);
    expect(get(reloaded.open(command, { name: "first" })).values.source).toBe(
      source,
    );
  });
  test("failed migration leaves the original aggregate available", () => {
    const command = commandById(V.update);
    const identity = panelKey(command, { name: "first" });
    const original = JSON.stringify({
      [identity]: { name: "first", source: "unfinished" },
    });
    const storage = new MemoryStorage();
    storage.setItem("loom.drafts.v1:http://one", original);
    const workspace = new Workspace();
    workspace.connect("http://one", {
      getItem: (key) => storage.getItem(key),
      setItem: () => {
        throw new Error("quota");
      },
      removeItem: (key) => storage.removeItem(key),
    });
    expect(get(workspace.storageError)).toContain("quota");
    expect(storage.getItem("loom.drafts.v1:http://one")).toBe(original);
  });
  test("drafts survive navigation and reload without leaking into another identity or endpoint", () => {
    const storage = new MemoryStorage();
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    const command = commandById(V.update);
    const first = workspace.open(command, { name: "first" });
    first.set({
      ...get(first),
      values: { name: "first", source: "unfinished Rust" },
    });
    const second = workspace.open(command, { name: "second" });
    expect(get(second).values.source).toBe("");
    expect(get(workspace.open(command, { name: "first" })).values.source).toBe(
      "unfinished Rust",
    );
    const reloaded = new Workspace();
    reloaded.connect("http://one", storage);
    expect(get(reloaded.open(command, { name: "first" })).values.source).toBe(
      "unfinished Rust",
    );
    reloaded.connect("http://two", storage);
    expect(get(reloaded.open(command, { name: "first" })).values.source).toBe(
      "",
    );
  });
  test("an empty source draft is not replaced by automatic source loading", async () => {
    const workspace = new Workspace();
    workspace.connect("http://one", new MemoryStorage());
    const session = workspace.open(commandById(V.update), {
      name: "first",
      source: "stored source",
    });
    session.set({
      ...get(session),
      values: { ...get(session).values, source: "" },
    });
    const client = new WorkbenchClient({
      request: async () => {
        throw new Error("Must not overwrite a draft");
      },
    });
    await session.prepare(client);
    expect(get(session).error).toBe("");
    expect(get(session).dirty).toBe(true);
    expect(get(session).values.source).toBe("");
  });
  test("pending execution stays with its panel, runs once, and completes in history after navigation", async () => {
    const workspace = new Workspace();
    workspace.connect("http://one", new MemoryStorage());
    let finish!: (value: unknown) => void;
    let calls = 0;
    const client = new WorkbenchClient({
      request: () => {
        calls++;
        return new Promise((resolve) => {
          finish = resolve;
        });
      },
    });
    const journal = new Journal();
    const command = commandById(V.run);
    const first = workspace.open(command, { target: "first", args: "21" });
    const completed: string[] = [];
    const running = first.execute(client, journal, (body) =>
      completed.push(String(body.target)),
    );
    const second = workspace.open(command, { target: "second" });
    expect(get(second).busy).toBe(false);
    const returned = workspace.open(command, { target: "first" });
    expect(get(returned).busy).toBe(true);
    await returned.execute(client, journal, () => {});
    expect(calls).toBe(1);
    expect(() => workspace.connect("http://two", new MemoryStorage())).toThrow(
      "running commands",
    );
    finish({
      ok: true,
      seq: 1,
      diagnostics: [],
      result: { output: 42, effects: [] },
    });
    await running;
    expect(get(returned).result).toEqual({ output: 42, effects: [] });
    expect(get(second).result).toBeUndefined();
    expect(completed).toEqual(["first"]);
    expect(get(journal)[0]?.state).toBe("completed");
  });
  test("storage failures are visible while drafts remain usable", () => {
    const workspace = new Workspace();
    workspace.connect("http://one", {
      getItem: () => null,
      removeItem: () => {},
      setItem: () => {
        throw new Error("quota");
      },
    });
    const session = workspace.open(commandById(V.add), {});
    session.set({
      ...get(session),
      values: { ...get(session).values, source: "draft" },
    });
    expect(get(workspace.storageError)).toContain("quota");
    expect(get(session).values.source).toBe("draft");
  });
  test("replay conflicts preserve drafts and are detected before mounting a form", async () => {
    const workspace = new Workspace();
    workspace.connect("http://one", new MemoryStorage());
    const command = commandById(V.add);
    const current = workspace.open(command, {});
    current.set({
      ...get(current),
      values: { ...get(current).values, name: "new", source: "unpublished" },
    });
    const old = { name: "old", source: "previously published" };
    expect(workspace.replayError(command, old)).toContain("unsaved draft");
    expect(() => workspace.open(command, old, true)).toThrow("unsaved draft");
    expect(get(workspace.open(command, {})).values.source).toBe("unpublished");
    let finish!: (value: unknown) => void;
    const run = workspace.open(commandById(V.run), {
      target: "one",
      args: "[]",
    });
    const pending = run.execute(
      new WorkbenchClient({
        request: () =>
          new Promise((resolve) => {
            finish = resolve;
          }),
      }),
      new Journal(),
      () => {},
    );
    expect(
      workspace.replayError(commandById(V.run), { target: "one" }),
    ).toContain("still running");
    finish({
      ok: true,
      seq: 1,
      diagnostics: [],
      result: { output: null, effects: [] },
    });
    await pending;
    expect(
      workspace.replayError(commandById(V.run), { target: "one" }),
    ).toBeNull();
  });
  test("malformed saved drafts do not get silently overwritten", () => {
    const storage = new MemoryStorage();
    storage.setItem("loom.drafts.v1:http://one", "bad JSON");
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    const session = workspace.open(commandById(V.add), {});
    session.set({
      ...get(session),
      values: { ...get(session).values, source: "draft" },
    });
    expect(get(workspace.storageError)).toContain("restore drafts");
    expect(storage.getItem("loom.drafts.v1:http://one")).toBe("bad JSON");
  });
});
test("active build status uses bearer authentication and validates actual phases", async () => {
  const requests: { url: string; authorization: string | null }[] = [];
  const client = new WorkbenchClient(
    new HttpTransport("http://one", "secret", async (url, init) => {
      requests.push({
        url,
        authorization: new Headers(init?.headers).get("Authorization"),
      });
      return new Response(
        JSON.stringify({
          ok: true,
          seq: 1,
          diagnostics: [],
          result: {
            active: { name: "first", stage: "compile", elapsed_ms: 1234 },
          },
        }),
      );
    }),
  );
  expect(await client.activeBuild()).toEqual({
    name: "first",
    stage: "compile",
    elapsed_ms: 1234,
  });
  expect(requests).toEqual([
    { url: "http://one/v1/builds/active", authorization: "Bearer secret" },
  ]);
});

describe("per-draft persistence", () => {
  const command = commandById(V.update);
  const indexKey = "loom.drafts.v1:http://one";
  function edit(workspace: Workspace, name: string, source: string) {
    const session = workspace.open(command, { name });
    session.set({
      ...get(session),
      values: { ...get(session).values, source },
    });
    return session;
  }
  test("editing one draft writes only that payload, without rewriting the index or other sources", () => {
    const storage = new MemoryStorage();
    const writes: { key: string; size: number }[] = [];
    const original = storage.setItem.bind(storage);
    storage.setItem = (key, value) => {
      writes.push({ key, size: value.length });
      original(key, value);
    };
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    for (let index = 0; index < 30; index++)
      edit(workspace, `item${index}`, "x".repeat(100000));
    writes.length = 0;
    edit(workspace, "item0", "changed");
    expect(writes).toHaveLength(1);
    expect(writes[0]!.key).not.toBe(indexKey);
    expect(writes[0]!.size).toBeLessThan(1000);
    const reloaded = new Workspace();
    reloaded.connect("http://one", storage);
    expect(get(reloaded.open(command, { name: "item0" })).values.source).toBe(
      "changed",
    );
    expect(
      get(reloaded.open(command, { name: "item29" })).values.source,
    ).toHaveLength(100000);
    edit(workspace, "item0", "");
    const afterRemoval = new Workspace();
    afterRemoval.connect("http://one", storage);
    expect(get(afterRemoval.open(command, { name: "item0" })).dirty).toBe(
      false,
    );
    expect(storage.values.size).toBe(30);
  });
  test("corrupt indexed payloads and missing payloads are preserved and block writes visibly", () => {
    for (const corruption of ["bad JSON", null]) {
      const storage = new MemoryStorage();
      const first = new Workspace();
      first.connect("http://one", storage);
      edit(first, "one", "original");
      const payload = [...storage.values.keys()].find(
        (key) => key !== indexKey,
      )!;
      if (corruption === null) storage.removeItem(payload);
      else storage.setItem(payload, corruption);
      const before = [...storage.values];
      const reloaded = new Workspace();
      reloaded.connect("http://one", storage);
      edit(reloaded, "one", "replacement");
      expect(get(reloaded.storageError)).toContain("restore drafts");
      expect([...storage.values]).toEqual(before);
    }
  });
  test("failed index insertion rolls back its payload and retains the in-memory draft", () => {
    const storage = new MemoryStorage();
    const original = storage.setItem.bind(storage);
    storage.setItem = (key, value) => {
      if (key === indexKey) throw new Error("index quota");
      original(key, value);
    };
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    const session = edit(workspace, "one", "unsaved");
    expect(get(session).values.source).toBe("unsaved");
    expect(get(workspace.storageError)).toContain("index quota");
    expect(storage.values.size).toBe(0);
    storage.setItem = original;
    edit(workspace, "one", "retry");
    expect(get(workspace.storageError)).toBe("");
    expect(storage.values.size).toBe(2);
  });
  test("failure saving one draft remains visible after another draft saves successfully", () => {
    const storage = new MemoryStorage();
    const workspace = new Workspace();
    workspace.connect("http://one", storage);
    edit(workspace, "one", "original");
    const original = storage.setItem.bind(storage);
    storage.setItem = (key, value) => {
      if (value.includes("rejected")) throw new Error("payload quota");
      original(key, value);
    };
    edit(workspace, "one", "rejected");
    edit(workspace, "two", "accepted");
    expect(get(workspace.storageError)).toContain("payload quota");
    const reloaded = new Workspace();
    reloaded.connect("http://one", storage);
    expect(get(reloaded.open(command, { name: "one" })).values.source).toBe(
      "original",
    );
  });
});

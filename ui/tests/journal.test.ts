import { V } from "../src/lib/workbench/commands";
import { describe, expect, test } from "bun:test";
import { get } from "svelte/store";
import {
  Journal,
  invocationSummary,
  parameterSummary,
  resultSummary,
} from "../src/lib/workbench/journal";
import { commandById } from "../src/lib/workbench/commands";

describe("session command history", () => {
  test("run previews show the invocation and output while retaining full trace data", () => {
    const journal = new Journal();
    const id = journal.begin(commandById(V.run), {
      target: "a".repeat(64),
      args: "21",
    });
    journal.finish(id, {
      entry: "double",
      output: 42,
      effects: [],
      scope: "trace-id",
    });
    const entry = get(journal)[0]!;
    expect(invocationSummary(entry)).toBe("double(21)");
    expect(resultSummary(entry)).toBe("→ 42");
    expect(entry.result).toMatchObject({ scope: "trace-id" });
  });
  test("retains immutable input and result snapshots in invocation order", () => {
    const journal = new Journal();
    const values = { args: "[42]" };
    const first = journal.begin(commandById(V.run), values);
    values.args = "[0]";
    const second = journal.begin(commandById(V.find), { text: "counter" });
    journal.finish(second, []);
    const result = { output: [42], effects: [] };
    journal.finish(first, result);
    result.output[0] = 0;
    expect(get(journal).map((entry) => entry.id)).toEqual([first, second]);
    expect(get(journal)[0]?.values.args).toBe("[42]");
    expect(get(journal)[0]?.result).toEqual({ output: [42], effects: [] });
    expect(get(journal).every((entry) => entry.state === "completed")).toBe(
      true,
    );
  });
  test("failed commands retain exact replay fields and a named error", () => {
    const journal = new Journal();
    const first = journal.begin(commandById(V.run), { args: "invalid" });
    journal.fail(first, "run.args: invalid JSON");
    expect(get(journal)[0]).toMatchObject({
      state: "failed",
      error: "run.args: invalid JSON",
      values: { args: "invalid" },
    });
    const replay = journal.begin(commandById(V.run), get(journal)[0]!.values);
    journal.finish(replay, null);
    expect(get(journal)).toHaveLength(2);
    expect(get(journal)[0]?.state).toBe("failed");
    expect(get(journal)[1]?.result).toBeNull();
  });
});

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    values,
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  };
}

describe("persistent endpoint history", () => {
  test("roundtrips snapshots and IDs while isolating endpoints and excluding URL credentials", () => {
    const storage = memoryStorage();
    const first = new Journal();
    first.connect("http://user:secret@localhost:8080/?token=hidden", storage);
    const id = first.begin(commandById(V.find), { text: "counter" });
    first.finish(id, ["counter"]);
    const restored = new Journal();
    restored.connect("http://localhost:8080", storage);
    expect(get(restored)).toEqual(get(first));
    expect(restored.begin(commandById(V.find), {})).toBe(id + 1);
    expect([...storage.values.keys()].join()).not.toContain("secret");
    expect([...storage.values.keys()].join()).not.toContain("hidden");
    const other = new Journal();
    other.connect("http://localhost:8081", storage);
    expect(get(other)).toEqual([]);
  });
  test("restored pending commands become interrupted and never rerun", () => {
    const storage = memoryStorage();
    const first = new Journal();
    first.connect("http://localhost", storage);
    first.begin(commandById(V.run), { args: "[1]" });
    expect(first.clear()).toBe(false);
    const restored = new Journal();
    restored.connect("http://localhost", storage);
    expect(get(restored)[0]).toMatchObject({
      state: "failed",
      error: "Interrupted before completion; the server outcome is unknown.",
    });
    expect(restored.clear()).toBe(true);
    const cleared = new Journal();
    cleared.connect("http://localhost", storage);
    expect(get(cleared)).toEqual([]);
    expect(cleared.begin(commandById(V.find), {})).toBe(2);
  });
  test("rejects malformed saved data with visible diagnostics", () => {
    for (const raw of [
      "broken",
      '{"version":1,"nextId":1,"entries":[{"id":1}]}',
    ]) {
      const storage = { getItem: () => raw, setItem: () => {} };
      const journal = new Journal();
      journal.connect("http://localhost", storage);
      expect(get(journal)).toEqual([]);
      expect(get(journal.storageError)).toContain("could not be restored");
    }
  });
  test("preserves corrupt saved bytes while new commands complete in memory", () => {
    for (const raw of [
      "broken",
      '{"version":1,"nextId":1,"entries":[{"id":1}]}',
    ]) {
      const storage = memoryStorage();
      const key = "repl-history:v1:http://localhost";
      storage.setItem(key, raw);
      const journal = new Journal();
      journal.connect("http://localhost", storage);
      const id = journal.begin(commandById(V.find), { text: "counter" });
      journal.finish(id, ["counter"]);
      expect(get(journal)[0]).toMatchObject({
        state: "completed",
        result: ["counter"],
      });
      expect(storage.getItem(key)).toBe(raw);
      expect(get(journal.storageError)).toContain("could not be restored");
    }
  });
  test("storage failures leave commands usable and expose the failure", () => {
    const journal = new Journal();
    journal.connect("http://localhost", {
      getItem: () => null,
      setItem: () => {
        throw new Error("quota exceeded");
      },
    });
    const id = journal.begin(commandById(V.find), {});
    journal.finish(id, []);
    expect(get(journal)[0]?.state).toBe("completed");
    expect(get(journal.storageError)).toContain("quota exceeded");
  });
  test("bounds completed history without dropping active commands or reusing IDs", () => {
    const journal = new Journal();
    const active = journal.begin(commandById(V.run), {});
    for (let index = 0; index < 110; index++) {
      const id = journal.begin(commandById(V.find), {});
      journal.finish(id, []);
    }
    expect(get(journal)).toHaveLength(101);
    expect(get(journal)[0]?.id).toBe(active);
    journal.finish(active, null);
    expect(get(journal)).toHaveLength(100);
    expect(get(journal).at(-1)?.id).toBe(111);
    expect(JSON.parse(journal.exportJson()).entries).toHaveLength(100);
  });
});

describe("history row summaries", () => {
  test("parameter summaries omit blank parameters; run rows keep the human invocation", () => {
    const journal = new Journal();
    const target = "85d8".padEnd(64, "0");
    const view = journal.begin(commandById(V.view), {
      target,
      actor: "",
      table: "",
      template: "",
      order_by: "",
    });
    journal.finish(view, {
      name: "counter",
      hash: "a41e908fc7a21f43bc7292fa4457641ecbab51e58d59a060393d243c2b7f9001",
    });
    const entry = get(journal)[0]!;
    expect(parameterSummary(entry)).toBe(`target=${target}`);
    expect(parameterSummary(entry)).not.toContain("actor=");
    expect(invocationSummary(entry)).toBe(parameterSummary(entry));
    expect(resultSummary(entry)).toBe("counter · a41e908f");
    const blank = journal.begin(commandById(V.find), { text: "" });
    journal.finish(blank, []);
    expect(parameterSummary(get(journal)[1]!)).toBe("");
    expect(resultSummary(get(journal)[1]!)).toBe("0 definitions");
  });
});

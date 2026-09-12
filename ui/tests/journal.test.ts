import { describe, expect, test } from "bun:test";
import { get } from "svelte/store";
import { Journal } from "../src/lib/workbench/journal";
import { commandById } from "../src/lib/workbench/commands";

describe("session command history", () => {
  test("retains immutable input and result snapshots in invocation order", () => {
    const journal = new Journal();
    const values = { args: "[42]" };
    const first = journal.begin(commandById("run"), values);
    values.args = "[0]";
    const second = journal.begin(commandById("find"), { query: "counter" });
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
    const first = journal.begin(commandById("run"), { args: "invalid" });
    journal.fail(first, "run.args: invalid JSON");
    expect(get(journal)[0]).toMatchObject({
      state: "failed",
      error: "run.args: invalid JSON",
      values: { args: "invalid" },
    });
    const replay = journal.begin(commandById("run"), get(journal)[0]!.values);
    journal.finish(replay, null);
    expect(get(journal)).toHaveLength(2);
    expect(get(journal)[0]?.state).toBe("failed");
    expect(get(journal)[1]?.result).toBeNull();
  });
});

import { describe, expect, test } from "bun:test";
import { V, commandById } from "../src/lib/workbench/commands";
import { fixtures, MockTransport } from "../src/lib/workbench/mock";
import {
  definition,
  definitionView,
  definitionDiff,
  history,
  object,
} from "../src/lib/workbench/schema";

describe("Rust result shapes", () => {
  test("definition item hashes and inferred rows retain their wire structure", () => {
    const view = definitionView(fixtures.definitions[0]);
    expect(view.items.counter).toBe(fixtures.definitions[0]!.items.counter);
    expect(view.entries.counter!.effects).toEqual({
      labels: [],
      unknown: false,
    });
    expect(
      definitionView(fixtures.definitions[1]).entries.forwarder!.effects,
    ).toEqual({ labels: ["sleep"], unknown: false });
    const pending = structuredClone(fixtures.definitions[0]!);
    pending.entries.counter!.effects.unknown = true;
    expect(definitionView(pending).entries.counter!.effects.unknown).toBe(true);
    expect(() =>
      definitionView({
        ...pending,
        entries: { counter: { effects: { labels: [], unknown: "yes" } } },
      }),
    ).toThrow("expected boolean");
    expect(() => definition({ ...pending, items: [] })).toThrow(
      "expected object",
    );
  });
  test("history carries timestamps and nullable changes; diff carries named item sets", () => {
    const revisions = history(fixtures.history);
    expect(revisions[0]!.changes).toBeNull();
    expect(revisions[1]!.timestamp).toBe(1789152130);
    const diff = definitionDiff(revisions[1]!.changes);
    expect(diff.old).toBe(revisions[0]!.hash);
    expect(diff.new).toBe(revisions[1]!.hash);
    expect(diff.added[0]!.name).toBe("increment");
    expect(diff.changed[0]!.old).toBe(fixtures.definitions[2]!.items.counter!);
    expect(diff.removed).toEqual([]);
  });
  test("mock successes and errors use the HTTP envelope", async () => {
    const transport = new MockTransport();
    const response = object(
      await transport.request(commandById(V.view), { target: "counter" }),
      "response",
    );
    expect(Object.keys(response).sort()).toEqual([
      "diagnostics",
      "ok",
      "result",
      "seq",
    ]);
    expect(response.ok).toBe(true);
    expect(response.seq).toBe(1);
    expect(response.diagnostics).toEqual([]);
    expect(definitionView(response.result).name).toBe("counter");
    const failure = object(
      await transport.request(commandById(V.view), { target: "missing" }),
      "response",
    );
    expect(failure.ok).toBe(false);
    expect(failure.diagnostics).toEqual([]);
    expect(object(failure.result, "error").code).toBe("operation_failed");
    expect(object(failure.result, "error").error).toContain("missing");
  });
});

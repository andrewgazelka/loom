import { describe, expect, test } from "bun:test";
import { get } from "svelte/store";
import { V, commandById, parseFields } from "../src/lib/workbench/commands";
import { parseResult, WorkbenchClient } from "../src/lib/workbench/client";
import { fixtures, MockTransport } from "../src/lib/workbench/mock";
import { repairFields, updateSession } from "../src/lib/workbench/schema";
import { Workspace } from "../src/lib/workbench/workspace";
import { Journal } from "../src/lib/workbench/journal";

const pending = {
  update: {
    id: "update-123",
    revision: 4,
    target: "increment",
    status: "needs_repair",
    changes: [],
    diagnostics: [
      {
        hash: "old-caller",
        names: ["caller"],
        source: "pub fn caller() {}",
        diagnostics: [{ message: "type mismatch" }],
        build: null,
      },
    ],
  },
};
describe("update repair workflow", () => {
  test("accepts a repair result without a published definition and preserves compiler diagnostics", () => {
    expect(parseResult(commandById(V.update), pending)).toEqual(pending);
    const session = updateSession(pending);
    expect(session.diagnostics[0]!.diagnostics).toEqual([
      { message: "type mismatch" },
    ]);
    const fields = repairFields(session);
    expect(parseFields(commandById(V.update_repair), fields)).toEqual({
      id: "update-123",
      revision: 4,
      changes: { caller: { source: "pub fn caller() {}" } },
    });
  });
  test("rejects malformed repair state and requests at the boundary", () => {
    expect(() =>
      updateSession({ update: { ...pending.update, revision: -1 } }),
    ).toThrow("nonnegative");
    expect(() =>
      updateSession({ update: { ...pending.update, status: "invented" } }),
    ).toThrow("unknown status");
    expect(() =>
      parseFields(commandById(V.update_repair), {
        id: "u",
        revision: "1",
        changes: '{"caller":{}}',
      }),
    ).toThrow("source");
    expect(() =>
      parseFields(commandById(V.update_repair), {
        id: "u",
        revision: "-1",
        changes: "{}",
      }),
    ).toThrow("nonnegative");
  });
  test("each repair revision has its own workspace so fresh diagnostics cannot reuse stale fields", () => {
    const workspace = new Workspace();
    const command = commandById(V.update_repair);
    const original = workspace.open(
      command,
      repairFields(updateSession(pending)),
    );
    const next = workspace.open(
      command,
      repairFields(
        updateSession({ update: { ...pending.update, revision: 5 } }),
      ),
    );
    expect(original).not.toBe(next);
    expect(get(original).values.revision).toBe("4");
    expect(get(next).values.revision).toBe("5");
  });
  test("source loading pins update to the observed definition and refreshes the guard after publication", async () => {
    const transport = new MockTransport();
    const newHash = "b".repeat(64);
    const client = new WorkbenchClient({
      request: async (command, body) => {
        const result = (await transport.request(command, body)) as {
          result: Record<string, unknown>;
        };
        if (command.operation === V.update) result.result.hash = newHash;
        return result;
      },
    });
    const workspace = new Workspace();
    const session = workspace.open(commandById(V.update), { name: "counter" });
    await session.prepare(client);
    expect(get(session).values.expected_hash).toBe(
      fixtures.definitions[0]!.hash,
    );
    expect(get(session).values.source).toBe(fixtures.definitions[0]!.source);
    let observed: unknown;
    await session.execute(client, new Journal(), (body) => {
      observed = body.expected_hash;
    });
    expect(observed).toBe(fixtures.definitions[0]!.hash);
    expect(get(session).error).toBe("");
    expect(get(session).values.expected_hash).toBe(newHash);
  });
  test("a failed repair advances the revision guard for the next edit", async () => {
    const workspace = new Workspace();
    const session = workspace.open(
      commandById(V.update_repair),
      repairFields(updateSession(pending)),
    );
    const client = new WorkbenchClient({
      request: async () => ({
        ok: true,
        seq: 1,
        diagnostics: [],
        result: { update: { ...pending.update, revision: 5 } },
      }),
    });
    await session.execute(client, new Journal(), () => {});
    expect(get(session).error).toBe("");
    expect(get(session).values.revision).toBe("5");
  });
});

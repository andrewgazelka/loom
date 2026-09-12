import { describe, expect, test } from "bun:test";
import {
  commands,
  commandById,
  parseFields,
} from "../src/lib/workbench/commands";
import {
  HttpTransport,
  parseResult,
  RequestSlot,
  WorkbenchClient,
} from "../src/lib/workbench/client";
import { fixtures, MockTransport } from "../src/lib/workbench/mock";
import {
  actor,
  actorTree,
  flattenTree,
  validation,
} from "../src/lib/workbench/schema";
const defaults = {
  hash: fixtures.definitions[0]!.hash,
  name: "counter",
  expected_hash: fixtures.definitions[0]!.hash,
  source: fixtures.definitions[0]!.source,
  before: fixtures.definitions[2]!.hash,
  after: fixtures.definitions[0]!.hash,
  id: "a0-counter",
  behavior_hash: fixtures.definitions[0]!.hash,
  candidate_hash: fixtures.definitions[2]!.hash,
  author: "operator",
  rationale: "Fixture replay",
  reason: "shutdown",
  old_hash: fixtures.definitions[0]!.hash,
  new_hash: fixtures.definitions[2]!.hash,
  at_seq: "40",
  group: "workers",
};
function values(id: string): Record<string, string> {
  const command = commandById(id),
    values: Record<string, string> = {};
  for (const field of command.fields) values[field.key] = field.initial ?? "";
  Object.assign(values, defaults);
  if (id === "actor_sql") values.query = "SELECT * FROM inbox ORDER BY seq";
  return values;
}
describe("operation contract", () => {
  test("eight definition and nineteen actor operations, all uniquely named", () => {
    expect(
      new Set(
        commands
          .filter((command) => command.group === "Definitions")
          .map((command) => command.operation),
      ).size,
    ).toBe(8);
    expect(
      new Set(
        commands
          .filter((command) => command.group === "Actors")
          .map((command) => command.operation),
      ).size,
    ).toBe(19);
    expect(new Set(commands.map((command) => command.id)).size).toBe(
      commands.length,
    );
  });
  for (const command of commands)
    test(`${command.name} fixture passes the live response boundary`, async () => {
      const result = await new WorkbenchClient(new MockTransport()).call(
        command,
        parseFields(command, values(command.id)),
      );
      expect(result).not.toBeUndefined();
    });
  test("unknown operation and fixture fail by name", async () => {
    expect(() => commandById("actor_delete")).toThrow("Unknown command");
    await expect(
      new WorkbenchClient(new MockTransport()).call(commandById("view"), {
        hash: "absent",
      }),
    ).rejects.toThrow("unknown definition absent");
  });
  test("HTTP sends canonical route, body and bearer authorization", async () => {
    let request: { url: string; options: RequestInit } | undefined;
    const transport = new HttpTransport(
      "http://localhost:8787/",
      "test-token",
      async (url, options) => {
        request = { url: String(url), options: options! };
        return new Response(JSON.stringify(fixtures.responses.actor_send));
      },
    );
    const body = { id: "a0-counter", key: "explicit-key", msg: { value: 43 } };
    await new WorkbenchClient(transport).call(commandById("actor_send"), body);
    expect(request?.url).toBe("http://localhost:8787/api/actors/actor_send");
    expect(request?.options.method).toBe("POST");
    expect(request?.options.headers).toEqual({
      "Content-Type": "application/json",
      Authorization: "Bearer test-token",
    });
    expect(JSON.parse(String(request?.options.body))).toEqual(body);
  });
  test("HTTP failures never use fixtures or normalize success envelopes", async () => {
    for (const failure of [
      { status: 401, body: "token required", match: "HTTP 401" },
      { status: 200, body: "<html>proxy</html>", match: "invalid JSON" },
      {
        status: 200,
        body: '{"ok":false,"error":"denied"}',
        match: "actor_send.cursor",
      },
    ]) {
      const client = new WorkbenchClient(
        new HttpTransport(
          "",
          "",
          async () => new Response(failure.body, { status: failure.status }),
        ),
      );
      await expect(
        client.call(commandById("actor_send"), { id: "a0-counter", msg: {} }),
      ).rejects.toThrow(failure.match);
    }
  });
  test("malformed lists, missing preimages and unsafe integers fail", () => {
    expect(() => parseResult(commandById("find"), {})).toThrow(
      "expected array",
    );
    expect(() =>
      parseResult(commandById("view"), {
        ...fixtures.definitions[0],
        items: [{ name: "entry", hash: "x" }],
      }),
    ).toThrow("preimage_size");
    expect(() =>
      parseResult(commandById("actor_send"), { cursor: 9007199254740992 }),
    ).toThrow("safe integer");
  });
});
describe("input validation", () => {
  test("empty search works, missing mutation fields fail", () => {
    expect(parseFields(commandById("find"), { query: "" })).toEqual({
      query: "",
    });
    expect(() =>
      parseFields(commandById("actor_promote"), { id: "a0-counter" }),
    ).toThrow("Behavior hash is required");
  });
  test("null initialization and omitted delivery key preserve MCP semantics", () => {
    expect(
      parseFields(commandById("actor_spawn"), {
        behavior_hash: "counter",
        init: "null",
      }),
    ).toEqual({ behavior_hash: "counter", init: null });
    expect(
      parseFields(commandById("actor_send"), {
        id: "a0-counter",
        msg: '{"n":1}',
      }),
    ).toEqual({ id: "a0-counter", msg: { n: 1 } });
  });
  test("rejects invalid JSON, non-string assertions and invalid replay windows", () => {
    expect(() =>
      parseFields(commandById("run"), { hash: "abc", args: "{" }),
    ).toThrow("Arguments JSON");
    expect(() =>
      parseFields(commandById("actor_validate"), {
        ...values("actor_validate"),
        assertions: "[false]",
      }),
    ).toThrow("SQL strings");
    expect(() =>
      parseFields(commandById("actor_validate"), {
        ...values("actor_validate"),
        k: "-1",
      }),
    ).toThrow("nonnegative");
    expect(() =>
      parseFields(commandById("actor_validate"), {
        ...values("actor_validate"),
        k: "9007199254740992",
      }),
    ).toThrow("safe integer");
  });
  test("table panels use actor_sql and ordered queries", () => {
    expect(
      parseFields(commandById("actor_effects"), { id: "a0-counter" }),
    ).toEqual({
      id: "a0-counter",
      query: "SELECT * FROM effects ORDER BY seq,idx",
    });
  });
});
describe("actor results", () => {
  for (const kind of ["Matched", "Differs", "DivergedAt", "Trapped"] as const)
    test(`${kind} keeps its details`, () => {
      expect(validation(fixtures.verdicts[kind]).verdict.kind).toBe(kind);
    });
  test("table hashes and assertion failures survive parsing", () => {
    const result = validation(fixtures.verdicts.Differs);
    expect(result.assertions[0]?.passed).toBe(false);
    expect(result.verdict).toEqual({
      kind: "Differs",
      tables: fixtures.verdicts.Differs.verdict.Differs.tables,
    });
  });
  test("divergence byte arrays remain exact; unknown variants fail", () => {
    expect(validation(fixtures.verdicts.DivergedAt).verdict).toEqual({
      kind: "DivergedAt",
      seq: 41,
      idx: 0,
      expected: [110, 111, 119],
      got: [101, 99, 104, 111],
    });
    expect(() =>
      validation({
        verdict: { DivergedAt: { seq: 1, idx: 0, expected: [256], got: [] } },
        assertions: [],
      }),
    ).toThrow("invalid byte");
    expect(() =>
      validation({ verdict: { Future: {} }, assertions: [] }),
    ).toThrow("unknown variant Future");
  });
  test("tree joins inbox lengths and rejects missing or duplicate identities", () => {
    const root = actorTree(fixtures.tree),
      actors = fixtures.actors.map(actor);
    const rows = flattenTree(root, actors);
    expect(rows.map((row) => ({ id: row.id, depth: row.depth }))).toEqual([
      { id: "a0-root", depth: 0 },
      { id: "a0-counter", depth: 1 },
      { id: "a0-forwarder", depth: 1 },
      { id: "a0-worker", depth: 2 },
    ]);
    expect(rows[1]?.inbox_len).toBe(2);
    expect(() => flattenTree(root, actors.slice(1))).toThrow(
      "missing tree actor a0-root",
    );
    expect(() =>
      flattenTree(
        { ...root, children: [...root.children, root.children[0]!] },
        actors,
      ),
    ).toThrow("duplicate actor");
  });
});
describe("request ownership", () => {
  test("late success cannot overwrite a newer selection", async () => {
    const slot = new RequestSlot();
    let resolveOld!: (value: string) => void;
    const received: string[] = [];
    const old = slot.run(
      () => new Promise<string>((resolve) => (resolveOld = resolve)),
      (value) => received.push(value),
      (error) => received.push(error),
      () => {},
    );
    await slot.run(
      async () => "new actor",
      (value) => received.push(value),
      (error) => received.push(error),
      () => {},
    );
    resolveOld("old actor");
    await old;
    expect(received).toEqual(["new actor"]);
  });
  test("disposing aborts transport and suppresses completion", async () => {
    const slot = new RequestSlot();
    let signal: AbortSignal | undefined;
    let finish!: () => void;
    let published = false;
    const pending = slot.run(
      (current) => {
        signal = current;
        return new Promise<void>((resolve) => (finish = resolve));
      },
      () => (published = true),
      () => (published = true),
      () => (published = true),
    );
    slot.cancel();
    finish();
    await pending;
    expect(signal?.aborted).toBe(true);
    expect(published).toBe(false);
  });
});

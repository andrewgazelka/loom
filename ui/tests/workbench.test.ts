import { V, panels } from "../src/lib/workbench/commands";
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
  target: fixtures.definitions[0]!.hash,
  name: "counter",
  source: fixtures.definitions[0]!.source,
  old: fixtures.definitions[2]!.hash,
  new: fixtures.definitions[0]!.hash,
  id: "a0-counter",
  def: fixtures.definitions[0]!.hash,
  candidate: fixtures.definitions[2]!.hash,
  author: "operator",
  rationale: "Fixture replay",
  reason: "shutdown",
  seq: "40",
  group: "workers",
};
function values(id: string): Record<string, string> {
  const command = commandById(id),
    values: Record<string, string> = {};
  for (const field of command.fields) values[field.key] = field.initial ?? "";
  Object.assign(values, defaults);
  if (id === V.sql) values.query = "SELECT * FROM inbox ORDER BY seq";
  return values;
}
describe("operation contract", () => {
  test("eight definition and twenty actor operations, all uniquely named", () => {
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
    ).toBe(20);
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
    expect(() => commandById("absent_operation")).toThrow("Unknown command");
    await expect(
      new WorkbenchClient(new MockTransport()).call(commandById(V.view), {
        target: "absent",
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
        return new Response(
          JSON.stringify({
            ok: true,
            seq: 1,
            result: { id: "a0-counter", seq: 43, cursor: 43 },
            diagnostics: [],
          }),
        );
      },
    );
    const body = { id: "a0-counter", key: "explicit-key", msg: { value: 43 } };
    await new WorkbenchClient(transport).call(commandById(V.send), body);
    expect(request?.url).toBe("http://localhost:8787/v1/command");
    expect(request?.options.method).toBe("POST");
    expect(request?.options.headers).toEqual({
      "Content-Type": "application/json",
      Authorization: "Bearer test-token",
    });
    expect(JSON.parse(String(request?.options.body))).toEqual({
      command: V.send,
      args: body,
    });
  });
  test("HTTP and protocol failures never use fixtures", async () => {
    for (const failure of [
      { status: 401, body: "token required", match: "HTTP 401" },
      { status: 200, body: "<html>proxy</html>", match: "invalid JSON" },
      {
        status: 200,
        body: '{"ok":false,"seq":1,"result":{"error":"denied","code":"forbidden"},"diagnostics":[]}',
        match: "denied",
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
        client.call(commandById(V.send), { id: "a0-counter", msg: {} }),
      ).rejects.toThrow(failure.match);
    }
  });
  test("malformed lists, missing effect rows and unsafe integers fail", () => {
    expect(() => parseResult(commandById(V.find), {})).toThrow(
      "expected array",
    );
    expect(() =>
      parseResult(commandById(V.view), {
        ...fixtures.definitions[0],
        entries: { entry: { item_hash: "x" } },
      }),
    ).toThrow("effects");
    expect(() =>
      parseResult(commandById(V.send), { cursor: 9007199254740992 }),
    ).toThrow("safe integer");
  });
});
describe("input validation", () => {
  test("empty search works, missing mutation fields fail", () => {
    expect(parseFields(commandById(V.find), { text: "" })).toEqual({
      text: "",
    });
    expect(() =>
      parseFields(commandById(V.promote), { id: "a0-counter" }),
    ).toThrow("Behavior hash is required");
  });
  test("null initialization and omitted delivery key preserve command semantics", () => {
    expect(
      parseFields(commandById(V.spawn), {
        def: "counter",
        init: "null",
      }),
    ).toEqual({ def: "counter", init: null });
    expect(
      parseFields(commandById(V.send), {
        id: "a0-counter",
        msg: '{"n":1}',
      }),
    ).toEqual({ id: "a0-counter", msg: { n: 1 } });
  });
  test("rejects invalid JSON, non-string assertions and invalid replay windows", () => {
    expect(() =>
      parseFields(commandById(V.run), { target: "abc", args: "{" }),
    ).toThrow("Arguments JSON");
    expect(() =>
      parseFields(commandById(V.validate), {
        ...values(V.validate),
        assertions: "[false]",
      }),
    ).toThrow("SQL strings");
    expect(() =>
      parseFields(commandById(V.validate), {
        ...values(V.validate),
        k: "-1",
      }),
    ).toThrow("nonnegative");
    expect(() =>
      parseFields(commandById(V.validate), {
        ...values(V.validate),
        k: "9007199254740992",
      }),
    ).toThrow("safe integer");
  });
  test("table panels use sql and ordered queries", () => {
    expect(
      parseFields(commandById(panels.effects), { id: "a0-counter" }),
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

describe("Rust verb table parity", () => {
  test("every public verb, argument, optional field and default matches the server", async () => {
    const source = await Bun.file(
      new URL("../../crates/loom-proto/src/verbs.rs", import.meta.url),
    ).text();
    const table = source
      .split("pub static VERBS: &[Verb] = &[")[1]!
      .split("\n];")[0]!;
    const server = [
      ...table.matchAll(
        /verb!\(\s*(\w+),\s*(Definition|Actor),\s*(\w+),\s*\[([\s\S]*?)\]\s*\)/g,
      ),
    ];
    expect(server.length).toBe(28);
    expect(
      commands
        .filter((command) => !command.query)
        .map((command) => String(command.operation))
        .sort(),
    ).toEqual(server.map((match) => match[1]!).sort());
    for (const match of server) {
      const command = commandById(match[1]!);
      const args = [
        ...match[4]!.matchAll(
          /arg!\(\s*(\w+),\s*(\w+)(?:,\s*(optional|flag|"[^"]*"))?\s*\)/g,
        ),
      ];
      expect(command.fields.map((field) => field.key).sort()).toEqual(
        args.map((arg) => arg[1]!).sort(),
      );
      expect(command.read).toBe(match[3] === "Read");
      for (const arg of args) {
        const field = command.fields.find((field) => field.key === arg[1])!;
        expect(Boolean(field.optional || field.default !== undefined)).toBe(
          arg[3] !== undefined && arg[3] !== "flag",
        );
        expect(field.default).toBe(
          arg[3]?.startsWith('"') ? JSON.parse(arg[3]) : undefined,
        );
        expect(
          field.kind === "source" ? "string" : (field.kind ?? "string"),
        ).toBe(
          (
            {
              Source: "string",
              Json: "json",
              Integer: "number",
              Count: "number",
              String: "string",
            } as Record<string, string>
          )[arg[2]!]!,
        );
      }
    }
  });
  test("defaults and count bounds match the wire contract", () => {
    expect(parseFields(commandById(V.spawn), { def: "counter" })).toEqual({
      def: "counter",
      init: null,
    });
    expect(parseFields(commandById(V.run), { target: "counter" })).toEqual({
      target: "counter",
      args: [],
    });
    expect(
      parseFields(commandById(V.validate), {
        id: "a",
        candidate: "h",
        k: "4294967295",
      }).k,
    ).toBe(4294967295);
    expect(() =>
      parseFields(commandById(V.validate), {
        id: "a",
        candidate: "h",
        k: "4294967296",
      }),
    ).toThrow("4294967295");
    expect(parseFields(commandById(V.fork), { id: "a", seq: "-1" }).seq).toBe(
      -1,
    );
  });
});

test("blank defaulted fields and nullable optional JSON preserve server semantics", () => {
  expect(
    parseFields(commandById(V.run), { target: "counter", args: "" }),
  ).toEqual({ target: "counter", args: [] });
  expect(
    parseFields(commandById(V.spawn), {
      def: "counter",
      init: "",
      spec: "null",
    }),
  ).toEqual({ def: "counter", init: null, spec: null });
  expect(
    parseFields(commandById(V.add), {
      source: "pub fn counter() {}",
      allowed_effects: "null",
    }).allowed_effects,
  ).toBeNull();
  expect(
    parseFields(commandById(V.validate), {
      id: "a",
      candidate: "h",
      k: "0",
      assertions: "null",
    }).assertions,
  ).toBeNull();
  expect(
    parseFields(commandById(V.sql), {
      id: "a",
      query: "SELECT 1",
      params: "null",
    }).params,
  ).toBeNull();
});

for (const args of [42, false, "hello", null, { n: 4 }, [1, 2], [[1, 2]]]) {
  test(`run preserves ${JSON.stringify(args)} through the HTTP boundary`, async () => {
    let sent: unknown;
    const client = new WorkbenchClient(
      new HttpTransport("", "", async (_url, options) => {
        sent = JSON.parse(String(options?.body));
        return new Response(
          JSON.stringify({
            ok: true,
            seq: 1,
            diagnostics: [],
            result: { output: args, effects: [] },
          }),
        );
      }),
    );
    const body = parseFields(commandById(V.run), {
      target: "echo",
      args: JSON.stringify(args),
    });
    await client.call(commandById(V.run), body);
    expect(sent).toEqual({ command: V.run, args: { target: "echo", args } });
  });
}

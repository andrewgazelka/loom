import { describe, expect, test } from "bun:test";
import { apply, applyTree, empty, type JournalEvent } from "../../src/lib/board/feed";
import type { Selection } from "../../src/lib/board/selection";
import { panes } from "../../src/lib/board/panes";
import {
  LAYOUT_KEY,
  compose,
  defaultArrangement,
  loadArrangement,
  saveArrangement,
  toggleHidden,
  toggleablePanes,
} from "../../src/lib/board/panes/arrangement";
import { tabs, tabsFor } from "../../src/lib/board/detail/tabs";

const DEF = "a".repeat(64);
const DEP = "b".repeat(64);
const COMPONENT = "c".repeat(64);
const LOGS = "d".repeat(64);

function event(seq: number, event: Record<string, unknown>): JournalEvent {
  return { seq, ts: 1_758_400_000 + seq, event };
}
/** One model with a definition, its dependency, a build, a completed call and an actor. */
function fixtureModel() {
  const folded = apply(empty(), [
    event(1, {
      type: "defined",
      name: "helper",
      def: { hash: DEP, lang: "rust", component_hash: null, sig: { exports: [{ name: "helper" }], effects: { labels: [] } } },
      deps: {},
    }),
    event(2, {
      type: "defined",
      name: "double",
      def: {
        hash: DEF,
        lang: "rust",
        component_hash: COMPONENT,
        sig: { exports: [{ name: "double" }], effects: { labels: ["call"], unknown: false } },
      },
      deps: { helper: DEP },
    }),
    event(3, { type: "component_built", component_hash: COMPONENT, logs_ref: LOGS, ms: 4321, size: 1024, rustc_invocations: 2 }),
    event(4, {
      type: "call_completed",
      scope: "call:1",
      definition_hash: DEF,
      args_hash: "e".repeat(64),
      trace_hash: "f".repeat(64),
      outcome: { status: "success" },
      elapsed_ms: 12,
      entry: "double",
    }),
    event(5, { type: "actor_message", actor: "worker", definition_hash: DEF, cursor: 3 }),
  ]);
  return applyTree(folded, {
    id: "root",
    status: "running",
    behavior_hash: DEF,
    cursor: 1,
    children: [{ id: "worker", status: "parked", behavior_hash: DEF, cursor: 3, children: [] }],
  });
}
const selections: (Selection | null)[] = [
  null,
  { kind: "def", hash: DEF },
  { kind: "actor", id: "worker" },
  { kind: "build", hash: COMPONENT },
  { kind: "run", scope: "call:1" },
];

function memoryStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  return {
    values,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
  };
}

describe("pane registry", () => {
  test("ids are unique and every pane names an area", () => {
    expect(new Set(panes.map((pane) => pane.id)).size).toBe(panes.length);
    for (const pane of panes) expect(["rail", "main"]).toContain(pane.area);
  });
  test("every pane's select runs against the fixture model for every selection it shows", () => {
    const model = fixtureModel();
    let calls = 0;
    for (const pane of panes)
      for (const selection of selections)
        if (pane.shows(selection)) {
          expect(() => pane.select(model, selection)).not.toThrow();
          calls++;
        }
    expect(calls).toBeGreaterThanOrEqual(panes.length);
  });
  test("each selection kind has exactly one main Detail pane, and the Overview has the rest", () => {
    for (const selection of selections.slice(1))
      expect(compose(panes, defaultArrangement(panes), selection).main).toHaveLength(1);
    const overview = compose(panes, defaultArrangement(panes), null);
    expect(overview.rail.map((pane) => pane.id)).toEqual(["definitions", "actors"]);
    expect(overview.main.map((cell) => cell.pane.id)).toEqual(["graph", "feed", "builds"]);
    expect(overview.main.map((cell) => cell.full)).toEqual([true, false, false]);
  });
  test("detail panes select against a selection their kind matches, and refuse the wrong kind", () => {
    const model = fixtureModel();
    const detail = panes.find((pane) => pane.id === "definition-detail")!;
    expect(detail.select(model, { kind: "def", hash: DEF })).toMatchObject({ def: { hash: DEF } });
    expect(() => detail.select(model, null)).toThrow();
  });
});

describe("board arrangement", () => {
  test("hiding a pane changes the persisted layout and the composed grid fills the space", () => {
    const storage = memoryStorage();
    const before = loadArrangement(storage, panes);
    expect(before).toEqual(defaultArrangement(panes));
    const after = toggleHidden(before, "builds");
    saveArrangement(storage, after);
    expect(JSON.parse(storage.values.get(LAYOUT_KEY)!)).toMatchObject({ version: 1, hidden: ["builds"] });
    const composed = compose(panes, after, null);
    expect(composed.main.map((cell) => [cell.pane.id, cell.full])).toEqual([
      ["graph", true],
      ["feed", true],
    ]);
    expect(loadArrangement(storage, panes)).toEqual(after);
    expect(toggleHidden(after, "builds").hidden).toEqual([]);
  });
  test("a saved layout is reconciled with the registry: unknown ids drop, new panes append", () => {
    const storage = memoryStorage({
      [LAYOUT_KEY]: JSON.stringify({
        version: 1,
        areas: { rail: ["actors", "retired"], main: ["feed"] },
        hidden: ["retired", "graph"],
      }),
    });
    const loaded = loadArrangement(storage, panes);
    expect(loaded.areas.rail).toEqual(["actors", "definitions"]);
    expect(loaded.areas.main[0]).toBe("feed");
    expect(loaded.hidden).toEqual(["graph"]);
    expect(compose(panes, loaded, null).rail.map((pane) => pane.id)).toEqual(["actors", "definitions"]);
  });
  test("malformed saved layouts fail by name", () => {
    expect(() => loadArrangement(memoryStorage({ [LAYOUT_KEY]: "{" }), panes)).toThrow("not JSON");
    expect(() =>
      loadArrangement(memoryStorage({ [LAYOUT_KEY]: JSON.stringify({ version: 2 }) }), panes),
    ).toThrow("unknown layout version");
    expect(() =>
      loadArrangement(
        memoryStorage({ [LAYOUT_KEY]: JSON.stringify({ version: 1, hidden: [1] }) }),
        panes,
      ),
    ).toThrow("hidden must be a list of pane ids");
  });
  test("the panes menu offers Overview panes only", () => {
    expect(toggleablePanes(panes).map((pane) => pane.id)).toEqual([
      "definitions",
      "actors",
      "graph",
      "feed",
      "builds",
    ]);
  });
});

describe("detail tab registry", () => {
  test("ids are unique and each selection kind gets its tabs in registry order", () => {
    expect(new Set(tabs.map((tab) => tab.id)).size).toBe(tabs.length);
    expect(tabsFor({ kind: "def", hash: DEF }).map((tab) => tab.id)).toEqual([
      "source",
      "wasm",
      "items",
      "history",
    ]);
    expect(tabsFor({ kind: "actor", id: "worker" }).map((tab) => tab.id)).toEqual(["tables", "lineage"]);
    expect(tabsFor({ kind: "build", hash: COMPONENT }).map((tab) => tab.id)).toEqual(["stages", "log"]);
    expect(tabsFor({ kind: "run", scope: "call:1" })).toEqual([]);
  });
  test("a tab's select narrows the context and refuses another kind", () => {
    const model = fixtureModel();
    const source = tabs.find((tab) => tab.id === "source")!;
    const props = source.select({
      kind: "def",
      def: model.definitions[DEF]!,
      view: null,
      viewError: null,
      client: null,
      onselect: () => {},
      names: {},
    });
    expect(props).toEqual({ lang: "rust", view: null, viewError: null });
    expect(() =>
      source.select({ kind: "actor", actor: model.actors.worker!, client: null }),
    ).toThrow("expected a def context, got actor");
  });
});

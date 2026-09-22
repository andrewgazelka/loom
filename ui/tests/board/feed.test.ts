import { describe, expect, test } from "bun:test";
import {
  ACTIVE_MS,
  FEED_CAP,
  apply,
  applyTree,
  buildOf,
  empty,
  nextExpiry,
  outcomeLabel,
  parseJournalEvent,
  prune,
  setStages,
  summarize,
  type JournalEvent,
} from "../../src/lib/board/feed";

const DEF = "a".repeat(64);
const DEP = "b".repeat(64);
const COMPONENT = "c".repeat(64);
const LOGS = "d".repeat(64);

function event(seq: number, event: Record<string, unknown>): JournalEvent {
  return { seq, ts: 1_758_400_000 + seq, event };
}
const defined = event(1, {
  type: "defined",
  name: "double",
  def: {
    hash: DEF,
    lang: "rust",
    component_hash: COMPONENT,
    sig: {
      exports: [{ name: "double", params: [], returns: { type: "number" } }],
      effects: { labels: ["call", "now"], unknown: false },
    },
  },
  source_hash: "e".repeat(64),
  deps: { helper: DEP },
  identity: null,
});
const built = event(2, {
  type: "component_built",
  component_hash: COMPONENT,
  logs_ref: LOGS,
  ms: 4321,
  size: 1024,
  rustc_invocations: 2,
});
const completed = event(3, {
  type: "call_completed",
  scope: "scope-1",
  definition_hash: DEF,
  args_hash: "f".repeat(64),
  outcome: { status: "success", result_hash: "0".repeat(64) },
  elapsed_ms: 12,
});

describe("board feed reducer", () => {
  test("defined, component_built and call_completed fold into one node with a build and an active mark", () => {
    const now = 1_000_000;
    const model = apply(empty(), [defined, built, completed], now);
    expect(Object.keys(model.definitions)).toEqual([DEF]);
    const def = model.definitions[DEF]!;
    expect(def.name).toBe("double");
    expect(def.exports).toEqual(["double"]);
    expect(def.effects).toEqual(["call", "now"]);
    expect(def.deps).toEqual({ helper: DEP });
    expect(buildOf(model, def)).toMatchObject({
      componentHash: COMPONENT,
      logsRef: LOGS,
      ms: 4321,
      rustcInvocations: 2,
      stages: null,
    });
    expect(model.activeUntil[DEF]).toBe(now + ACTIVE_MS);
    expect(nextExpiry(model)).toBe(now + ACTIVE_MS);
    expect(model.seq).toBe(3);
    expect(model.feed.map((row) => row.seq)).toEqual([3, 2, 1]);
    const summary = summarize(model, model.feed[0]!);
    expect(summary).toMatchObject({ name: "double", outcome: "ok", elapsedMs: 12 });
  });

  test("a build recorded before its definition still joins it", () => {
    const model = apply(empty(), [{ ...built, seq: 1 }, { ...defined, seq: 2 }]);
    expect(buildOf(model, model.definitions[DEF]!)?.componentHash).toBe(COMPONENT);
    expect(summarize(model, model.feed[1]!).name).toBe("double");
  });

  test("a replayed or reordered seq is dropped, so rows stay unique", () => {
    const model = apply(empty(), [defined, built, built, defined, completed]);
    expect(model.feed.map((row) => row.seq)).toEqual([3, 2, 1]);
    expect(apply(model, [completed]).feed).toHaveLength(3);
  });

  test("an unknown type is kept as a generic feed row and touches nothing else", () => {
    const model = apply(empty(), [event(9, { type: "something_new", payload: 1 })]);
    expect(model.feed).toHaveLength(1);
    expect(model.feed[0]).toMatchObject({ seq: 9, type: "something_new" });
    expect(Object.keys(model.definitions)).toHaveLength(0);
    expect(Object.keys(model.builds)).toHaveLength(0);
    expect(model.activeUntil).toEqual({});
    const typeless = apply(model, [event(10, { payload: 2 })]);
    expect(typeless.feed[0]?.type).toBe("event");
  });

  test("the feed holds the newest 500 rows", () => {
    const events = Array.from({ length: FEED_CAP + 25 }, (_, index) =>
      event(index + 1, { type: "effect_recorded", scope: `s${index}` }),
    );
    const model = apply(empty(), events);
    expect(model.feed).toHaveLength(FEED_CAP);
    expect(model.feed[0]?.seq).toBe(FEED_CAP + 25);
    expect(model.feed[FEED_CAP - 1]?.seq).toBe(26);
    expect(model.seq).toBe(FEED_CAP + 25);
  });

  test("the reducer never mutates its input", () => {
    const before = apply(empty(), [defined]);
    const snapshot = JSON.stringify(before);
    apply(before, [built, completed]);
    expect(JSON.stringify(before)).toBe(snapshot);
  });

  test("active marks expire through prune and stay put before their time", () => {
    const now = 5_000;
    const model = apply(empty(), [defined, completed], now);
    expect(prune(model, now + ACTIVE_MS - 1)).toBe(model);
    expect(prune(model, now + ACTIVE_MS).activeUntil).toEqual({});
  });

  test("outcome labels follow TraceOutcome's status tag", () => {
    expect(outcomeLabel({ status: "success", result_hash: "x" })).toBe("ok");
    expect(outcomeLabel({ status: "error", message: "boom" })).toBe("error");
    expect(outcomeLabel({ status: "cancelled" })).toBe("cancelled");
    expect(outcomeLabel(null)).toBeNull();
    expect(outcomeLabel({})).toBe("unknown");
  });

  test("a tree poll replaces the actor set and marks changed cursors; actor_message applies at once", () => {
    const tree = {
      id: "root",
      status: "running",
      behavior_hash: DEF,
      cursor: 3,
      children: [
        { id: "worker", status: "parked", behavior_hash: DEP, cursor: 7, children: [] },
      ],
    };
    const first = applyTree(apply(empty(), [defined]), tree, 100);
    expect(first.actorOrder).toEqual(["root", "worker"]);
    expect(first.actors.worker).toMatchObject({ depth: 1, parent: "root", cursor: 7 });
    expect(first.bumpedUntil).toEqual({});

    const pushed = apply(
      first,
      [event(4, { type: "actor_message", actor: "worker", definition_hash: DEP, cursor: 8 })],
      200,
    );
    expect(pushed.actors.worker?.cursor).toBe(8);
    expect(pushed.bumpedUntil.worker).toBe(200 + ACTIVE_MS);
    expect(summarize(pushed, pushed.feed[0]!)).toMatchObject({ actor: "worker", cursor: 8 });

    const reconciled = applyTree(
      pushed,
      { ...tree, children: [{ ...tree.children[0]!, cursor: 9 }] },
      300,
    );
    expect(reconciled.actors.worker?.cursor).toBe(9);
    expect(reconciled.bumpedUntil.worker).toBe(300 + ACTIVE_MS);
    expect(() => applyTree(reconciled, { id: "root", children: [] }, 0)).toThrow(
      "actor tree: root has no cursor",
    );
  });

  test("stage outcomes attach to the build they were fetched for", () => {
    const model = apply(empty(), [built]);
    const stages = setStages(model, COMPONENT, { compile: 4000, link: 321 });
    expect(stages.builds[COMPONENT]?.stages).toEqual({ compile: 4000, link: 321 });
    const failed = setStages(stages, COMPONENT, new Error("HTTP 404"));
    expect(failed.builds[COMPONENT]).toMatchObject({ stages: null, stagesError: "HTTP 404" });
    expect(() => setStages(model, DEP, {})).toThrow("is not in the model");
  });

  test("wire rows are validated before they reach the reducer", () => {
    expect(parseJournalEvent({ seq: 1, ts: 2, event: { type: "x" } })).toEqual({
      seq: 1,
      ts: 2,
      event: { type: "x" },
    });
    expect(() => parseJournalEvent({ ts: 2, event: {} })).toThrow("seq");
    expect(() => parseJournalEvent({ seq: 1, event: {} })).toThrow("ts");
    expect(() => parseJournalEvent({ seq: 1, ts: 2 })).toThrow("event");
  });
});

import { describe, expect, test } from "bun:test";
import { apply, empty, type JournalEvent } from "../../src/lib/board/feed";
import {
  HOST,
  ISOLATED,
  NODE_HEIGHT,
  NODE_WIDTH,
  graphOf,
  layout,
  type GraphEdge,
  type GraphNode,
} from "../../src/lib/board/layout";

const hash = (letter: string) => letter.repeat(64);
function node(id: string, label = id): GraphNode {
  return { id, label, detail: id.slice(0, 8), synthetic: false };
}
function edge(from: string, to: string): GraphEdge {
  return { from, to, kind: "static" };
}

describe("board layout", () => {
  test("a diamond yields three layers, dependencies left, and a name-sorted middle row", () => {
    // top depends on left and right; both depend on base.
    const nodes = [node("top"), node("right"), node("left"), node("base")];
    const edges = [
      edge("top", "right"),
      edge("top", "left"),
      edge("left", "base"),
      edge("right", "base"),
    ];
    const result = layout(nodes, edges);
    const layers = Object.fromEntries(result.nodes.map((item) => [item.id, item.layer]));
    expect(layers).toEqual({ base: 0, left: 1, right: 1, top: 2 });
    expect(new Set(result.nodes.map((item) => item.layer)).size).toBe(3);
    const middle = result.nodes.filter((item) => item.layer === 1);
    expect(middle.map((item) => item.id)).toEqual(["left", "right"]);
    expect(middle[0]!.y).toBeLessThan(middle[1]!.y);
    expect(result.edges).toHaveLength(4);
    expect(result.width).toBe(3 * NODE_WIDTH + 2 * 72);
  });

  test("two runs over shuffled input produce identical output", () => {
    const nodes = [node("top"), node("right"), node("left"), node("base")];
    const edges = [
      edge("top", "right"),
      edge("top", "left"),
      edge("left", "base"),
      edge("right", "base"),
    ];
    const first = layout(nodes, edges);
    const second = layout([...nodes].reverse(), [...edges].reverse());
    expect(JSON.stringify(second.nodes)).toBe(JSON.stringify(first.nodes));
    expect(new Set(second.edges.map((item) => item.path))).toEqual(
      new Set(first.edges.map((item) => item.path)),
    );
  });

  test("sixty nodes never overlap", () => {
    const nodes = Array.from({ length: 60 }, (_, index) => node(`n${String(index).padStart(2, "0")}`));
    const edges: GraphEdge[] = [];
    for (let index = 1; index < 60; index++)
      edges.push(edge(`n${String(index).padStart(2, "0")}`, `n${String(Math.floor(index / 3)).padStart(2, "0")}`));
    const result = layout(nodes, edges);
    for (const a of result.nodes)
      for (const b of result.nodes) {
        if (a === b) continue;
        const apart =
          a.x + NODE_WIDTH <= b.x ||
          b.x + NODE_WIDTH <= a.x ||
          a.y + NODE_HEIGHT <= b.y ||
          b.y + NODE_HEIGHT <= a.y;
        expect(apart).toBe(true);
      }
  });

  test("a dependency cycle terminates: the back edge adds no layer, both nodes and edges are placed", () => {
    const result = layout([node("a"), node("b")], [edge("a", "b"), edge("b", "a")]);
    expect(result.nodes.map((item) => [item.id, item.layer])).toEqual([
      ["b", 1],
      ["a", 2],
    ]);
    expect(result.edges).toHaveLength(2);
    expect(result.edges.every((item) => item.path.startsWith("M"))).toBe(true);
  });

  test("graphOf turns effect labels into synthetic targets with the expected edge kinds", () => {
    const events: JournalEvent[] = [
      {
        seq: 1,
        ts: 0,
        event: {
          type: "defined",
          name: "leaf",
          def: { hash: hash("b"), lang: "rust", sig: { exports: [], effects: { labels: ["now", "sleep"] } } },
          deps: {},
        },
      },
      {
        seq: 2,
        ts: 0,
        event: {
          type: "defined",
          name: "caller",
          def: { hash: hash("a"), lang: "rust", sig: { exports: [], effects: { labels: ["call"] } } },
          deps: { leaf: hash("b"), missing: hash("f") },
        },
      },
    ];
    const graph = graphOf(apply(empty(), events));
    expect(graph.nodes.map((item) => item.id)).toEqual([hash("a"), hash("b"), ISOLATED, HOST]);
    expect(graph.edges).toEqual([
      { from: hash("a"), to: hash("b"), kind: "static" },
      { from: hash("a"), to: ISOLATED, kind: "isolated" },
      { from: hash("b"), to: HOST, kind: "host" },
    ]);
    const placed = layout(graph.nodes, graph.edges);
    const layers = new Map(placed.nodes.map((item) => [item.id, item.layer]));
    expect(layers.get(hash("b"))).toBe(0);
    expect(layers.get(hash("a"))).toBe(1);
    expect(layers.get(ISOLATED)).toBe(2);
    expect(layers.get(HOST)).toBe(2);
  });
});

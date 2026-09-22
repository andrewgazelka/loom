/** Deterministic layered layout of the definition graph. No overlaps by construction: one column per layer, rows stacked by name. */
import type { Model } from "./feed";

export type EdgeKind = "static" | "isolated" | "host";
export interface GraphNode {
  id: string;
  label: string;
  /** Second line under the label: the 8-char hash, empty for synthetic nodes. */
  detail: string;
  synthetic: boolean;
}
export interface GraphEdge {
  from: string;
  to: string;
  kind: EdgeKind;
}
export interface PlacedNode extends GraphNode {
  layer: number;
  x: number;
  y: number;
}
export interface PlacedEdge extends GraphEdge {
  path: string;
}
export interface Layout {
  nodes: PlacedNode[];
  edges: PlacedEdge[];
  width: number;
  height: number;
}

export const NODE_WIDTH = 176;
export const NODE_HEIGHT = 42;
export const COLUMN_GAP = 72;
export const ROW_GAP = 12;
export const ISOLATED = "isolated";
export const HOST = "host";

/** Nodes are definitions; static edges point from a definition to each dependency; effect labels add synthetic targets. */
export function graphOf(model: Model): { nodes: GraphNode[]; edges: GraphEdge[] } {
  const definitions = Object.values(model.definitions).sort((a, b) =>
    a.hash < b.hash ? -1 : a.hash > b.hash ? 1 : 0,
  );
  const nodes: GraphNode[] = definitions.map((def) => ({
    id: def.hash,
    label: def.name ?? def.hash.slice(0, 8),
    detail: def.hash.slice(0, 8),
    synthetic: false,
  }));
  const edges: GraphEdge[] = [];
  let isolated = false;
  let host = false;
  for (const def of definitions) {
    for (const target of Object.values(def.deps))
      if (model.definitions[target] && target !== def.hash)
        edges.push({ from: def.hash, to: target, kind: "static" });
    const labels = [...new Set(def.effects)].sort();
    for (const label of labels) {
      if (label === "call") {
        isolated = true;
        edges.push({ from: def.hash, to: ISOLATED, kind: "isolated" });
      } else {
        host = true;
        edges.push({ from: def.hash, to: HOST, kind: "host" });
      }
    }
  }
  // One host edge per definition, whatever the number of labels.
  const seen = new Set<string>();
  const unique = edges.filter((edge) => {
    const key = `${edge.kind}\n${edge.from}\n${edge.to}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  if (isolated)
    nodes.push({ id: ISOLATED, label: "isolated call", detail: "", synthetic: true });
  if (host) nodes.push({ id: HOST, label: "host", detail: "", synthetic: true });
  return { nodes, edges: unique };
}

/** Longest-path layering: a definition sits one layer right of its deepest dependency; synthetic targets take the last column. */
export function layout(nodes: GraphNode[], edges: GraphEdge[]): Layout {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const dependencies = new Map<string, string[]>();
  for (const edge of edges) {
    if (edge.kind !== "static" || !byId.has(edge.from) || !byId.has(edge.to))
      continue;
    const list = dependencies.get(edge.from) ?? [];
    list.push(edge.to);
    dependencies.set(edge.from, list);
  }
  const layers = new Map<string, number>();
  const visiting = new Set<string>();
  const layerOf = (id: string): number => {
    const known = layers.get(id);
    if (known !== undefined) return known;
    if (visiting.has(id)) return 0; // cycle: the back edge does not add a layer
    visiting.add(id);
    let depth = 0;
    for (const target of dependencies.get(id) ?? [])
      depth = Math.max(depth, layerOf(target) + 1);
    visiting.delete(id);
    layers.set(id, depth);
    return depth;
  };
  let deepest = -1;
  for (const node of nodes)
    if (!node.synthetic) deepest = Math.max(deepest, layerOf(node.id));
  for (const node of nodes) if (node.synthetic) layers.set(node.id, deepest + 1);

  const columns = new Map<number, GraphNode[]>();
  for (const node of nodes) {
    const layer = layers.get(node.id) ?? 0;
    const column = columns.get(layer) ?? [];
    column.push(node);
    columns.set(layer, column);
  }
  const placed: PlacedNode[] = [];
  const position = new Map<string, PlacedNode>();
  let width = 0;
  let height = 0;
  for (const [layer, column] of [...columns.entries()].sort((a, b) => a[0] - b[0])) {
    column.sort((a, b) =>
      a.label < b.label ? -1 : a.label > b.label ? 1 : a.id < b.id ? -1 : a.id > b.id ? 1 : 0,
    );
    column.forEach((node, index) => {
      const item: PlacedNode = {
        ...node,
        layer,
        x: layer * (NODE_WIDTH + COLUMN_GAP),
        y: index * (NODE_HEIGHT + ROW_GAP),
      };
      placed.push(item);
      position.set(node.id, item);
      width = Math.max(width, item.x + NODE_WIDTH);
      height = Math.max(height, item.y + NODE_HEIGHT);
    });
  }
  const placedEdges: PlacedEdge[] = [];
  for (const edge of edges) {
    const from = position.get(edge.from);
    const to = position.get(edge.to);
    if (!from || !to) continue;
    placedEdges.push({ ...edge, path: curve(from, to) });
  }
  return { nodes: placed, edges: placedEdges, width, height };
}

function curve(from: PlacedNode, to: PlacedNode): string {
  const middle = NODE_HEIGHT / 2;
  if (to.x > from.x) {
    const x1 = from.x + NODE_WIDTH,
      y1 = from.y + middle,
      x2 = to.x,
      y2 = to.y + middle;
    const bend = (x2 - x1) / 2;
    return `M${x1},${y1} C${x1 + bend},${y1} ${x2 - bend},${y2} ${x2},${y2}`;
  }
  if (to.x < from.x) {
    const x1 = from.x,
      y1 = from.y + middle,
      x2 = to.x + NODE_WIDTH,
      y2 = to.y + middle;
    const bend = (x1 - x2) / 2;
    return `M${x1},${y1} C${x1 - bend},${y1} ${x2 + bend},${y2} ${x2},${y2}`;
  }
  // Same column: layering keeps static edges between columns, so this is the defensive fallback.
  const x = from.x + NODE_WIDTH / 2;
  return `M${x},${from.y + middle} L${x},${to.y + middle}`;
}

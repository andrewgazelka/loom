export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };
export type Row = { [key: string]: Json };
export interface Definition {
  name: string;
  hash: string;
  entry_item_hash: string | null;
  updated: string;
}
export interface Item {
  name: string;
  hash: string;
  preimage_size: number;
  refs: string[];
}
export interface DefinitionView extends Definition {
  source: string;
  items: Item[];
}
export interface Change {
  name: string;
  before: string | null;
  after: string | null;
}
export interface Revision {
  hash: string;
  parent_hash: string | null;
  updated: string;
  changed_items: Change[];
}
export interface DefinitionDiff {
  before: string;
  after: string;
  source_before: string;
  source_after: string;
  changed_items: Change[];
}
export interface RunResult {
  output: Json;
  effects: Row[];
}
export interface Actor {
  id: string;
  status: string;
  behavior_hash: string;
  cursor: number;
  inbox_len: number;
  parent: string | null;
}
export interface ActorNode {
  id: string;
  status: string;
  behavior_hash: string;
  cursor: number;
  children: ActorNode[];
}
export interface TreeRow extends Actor {
  depth: number;
}
export interface TableHash {
  name: string;
  hash: string;
}
export interface TableDifference {
  name: string;
  original_hash: string;
  fork_hash: string;
}
export type Verdict =
  | { kind: "Matched"; tables: TableHash[] }
  | { kind: "Differs"; tables: TableDifference[] }
  | {
      kind: "DivergedAt";
      seq: number;
      idx: number;
      expected: number[];
      got: number[];
    }
  | { kind: "Trapped"; seq: number; error: string };
export interface Validation {
  verdict: Verdict;
  assertions: { query: string; passed: boolean }[];
}
export function object(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    throw new Error(`${path}: expected object`);
  return value as Record<string, unknown>;
}
export function array(value: unknown, path: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`${path}: expected array`);
  return value;
}
export function string(value: unknown, path: string): string {
  if (typeof value !== "string") throw new Error(`${path}: expected string`);
  return value;
}
export function integer(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value))
    throw new Error(`${path}: expected safe integer`);
  return value;
}
export function nullableString(value: unknown, path: string): string | null {
  return value === null ? null : string(value, path);
}
export function json(value: unknown, path: string): Json {
  if (value === null || typeof value === "string" || typeof value === "boolean")
    return value;
  if (
    typeof value === "number" &&
    Number.isFinite(value) &&
    (!Number.isInteger(value) || Number.isSafeInteger(value))
  )
    return value;
  if (Array.isArray(value))
    return value.map((item, index) => json(item, `${path}[${index}]`));
  const result: Row = {};
  for (const key of Object.keys(object(value, path))) {
    Object.defineProperty(result, key, {
      value: json(object(value, path)[key], `${path}.${key}`),
      enumerable: true,
      writable: true,
    });
  }
  return result;
}
export function rows(value: unknown, path: string): Row[] {
  return array(value, path).map(
    (item, index) => json(object(item, `${path}[${index}]`), path) as Row,
  );
}
export function definition(value: unknown): Definition {
  const data = object(value, "definition");
  return {
    name: string(data.name, "definition.name"),
    hash: string(data.hash, "definition.hash"),
    entry_item_hash: nullableString(
      data.entry_item_hash,
      "definition.entry_item_hash",
    ),
    updated: string(data.updated, "definition.updated"),
  };
}
export function definitionView(value: unknown): DefinitionView {
  const data = object(value, "view");
  return {
    ...definition(value),
    source: string(data.source, "view.source"),
    items: array(data.items, "view.items").map((value) => {
      const item = object(value, "item");
      const size = integer(item.preimage_size, "item.preimage_size");
      if (size < 0) throw new Error("item.preimage_size: must be nonnegative");
      return {
        name: string(item.name, "item.name"),
        hash: string(item.hash, "item.hash"),
        preimage_size: size,
        refs: array(item.refs, "item.refs").map((value) =>
          string(value, "item.refs[]"),
        ),
      };
    }),
  };
}
export function changes(value: unknown): Change[] {
  return array(value, "changed_items").map((value) => {
    const item = object(value, "changed_item");
    return {
      name: string(item.name, "changed_item.name"),
      before: nullableString(item.before, "changed_item.before"),
      after: nullableString(item.after, "changed_item.after"),
    };
  });
}
export function history(value: unknown): Revision[] {
  return array(value, "history").map((value) => {
    const item = object(value, "history entry");
    return {
      hash: string(item.hash, "history.hash"),
      parent_hash: nullableString(item.parent_hash, "history.parent_hash"),
      updated: string(item.updated, "history.updated"),
      changed_items: changes(item.changed_items),
    };
  });
}
export function definitionDiff(value: unknown): DefinitionDiff {
  const data = object(value, "diff");
  return {
    before: string(data.before, "diff.before"),
    after: string(data.after, "diff.after"),
    source_before: string(data.source_before, "diff.source_before"),
    source_after: string(data.source_after, "diff.source_after"),
    changed_items: changes(data.changed_items),
  };
}
export function actor(value: unknown): Actor {
  const data = object(value, "actor");
  return {
    id: string(data.id, "actor.id"),
    status: string(data.status, "actor.status"),
    behavior_hash: string(data.behavior_hash, "actor.behavior_hash"),
    cursor: integer(data.cursor, "actor.cursor"),
    inbox_len: integer(data.inbox_len, "actor.inbox_len"),
    parent: nullableString(data.parent, "actor.parent"),
  };
}
export function actorTree(value: unknown, depth = 0): ActorNode {
  if (depth > 128) throw new Error("actor_tree: depth exceeds 128");
  const data = object(value, "actor_tree");
  return {
    id: string(data.id, "actor_tree.id"),
    status: string(data.status, "actor_tree.status"),
    behavior_hash: string(data.behavior_hash, "actor_tree.behavior_hash"),
    cursor: integer(data.cursor, "actor_tree.cursor"),
    children: array(data.children, "actor_tree.children").map((value) =>
      actorTree(value, depth + 1),
    ),
  };
}
export function flattenTree(root: ActorNode, actors: Actor[]): TreeRow[] {
  const byId = new Map<string, Actor>();
  for (const actor of actors) byId.set(actor.id, actor);
  const seen = new Set<string>();
  const result: TreeRow[] = [];
  function visit(node: ActorNode, depth: number) {
    if (seen.has(node.id))
      throw new Error(`actor_tree: duplicate actor ${node.id}`);
    seen.add(node.id);
    const actor = byId.get(node.id);
    if (!actor) throw new Error(`actor_list: missing tree actor ${node.id}`);
    result.push({ ...actor, depth });
    node.children.forEach((child) => visit(child, depth + 1));
  }
  visit(root, 0);
  return result;
}
export function validation(value: unknown): Validation {
  const data = object(value, "actor_validate");
  const tagged = object(data.verdict, "verdict");
  if (Object.keys(tagged).length !== 1)
    throw new Error("verdict: expected one variant");
  const kind = Object.keys(tagged)[0]!;
  const detail = object(tagged[kind], `verdict.${kind}`);
  let verdict: Verdict;
  if (kind === "Matched")
    verdict = {
      kind,
      tables: array(detail.tables, "Matched.tables").map((value) => {
        const table = object(value, "table");
        return {
          name: string(table.name, "table.name"),
          hash: string(table.hash, "table.hash"),
        };
      }),
    };
  else if (kind === "Differs")
    verdict = {
      kind,
      tables: array(detail.tables, "Differs.tables").map((value) => {
        const table = object(value, "table");
        return {
          name: string(table.name, "table.name"),
          original_hash: string(table.original_hash, "table.original_hash"),
          fork_hash: string(table.fork_hash, "table.fork_hash"),
        };
      }),
    };
  else if (kind === "DivergedAt") {
    const bytes = (value: unknown, path: string) =>
      array(value, path).map((value) => {
        const byte = integer(value, path);
        if (byte < 0 || byte > 255) throw new Error(`${path}: invalid byte`);
        return byte;
      });
    verdict = {
      kind,
      seq: integer(detail.seq, "DivergedAt.seq"),
      idx: integer(detail.idx, "DivergedAt.idx"),
      expected: bytes(detail.expected, "DivergedAt.expected"),
      got: bytes(detail.got, "DivergedAt.got"),
    };
  } else if (kind === "Trapped")
    verdict = {
      kind,
      seq: integer(detail.seq, "Trapped.seq"),
      error: string(detail.error, "Trapped.error"),
    };
  else throw new Error(`verdict: unknown variant ${kind}`);
  return {
    verdict,
    assertions: array(data.assertions, "assertions").map((value) => {
      const result = object(value, "assertion");
      if (typeof result.passed !== "boolean")
        throw new Error("assertion.passed: expected boolean");
      return {
        query: string(result.query, "assertion.query"),
        passed: result.passed,
      };
    }),
  };
}

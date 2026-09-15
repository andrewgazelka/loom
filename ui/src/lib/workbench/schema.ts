export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };
export type Row = { [key: string]: Json };
export interface Definition {
  name: string | null;
  hash: string;
  items: Record<string, string>;
}
export interface EffectRow {
  labels: string[];
  unknown: boolean;
}
export interface DefinitionView extends Definition {
  source: string;
  def: Row;
  behavior_hash: string;
  wasm_hash?: string;
  toolchain_hash?: string;
  entries: Record<string, { effects: EffectRow }>;
}
export interface Change {
  name: string;
  old: string;
  new: string;
}
export interface DefinitionDiff {
  old: string;
  new: string;
  added: { name: string; hash: string }[];
  removed: { name: string; hash: string }[];
  changed: Change[];
}
export interface Revision {
  name: string;
  hash: string;
  timestamp: number;
  changes: DefinitionDiff | null;
}
export interface RunResult {
  hash: string;
  output: Json;
  scope: string;
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
function hashes(value: unknown, path: string): Record<string, string> {
  const data = object(value, path);
  return Object.fromEntries(
    Object.keys(data).map((name) => [
      name,
      string(data[name], `${path}.${name}`),
    ]),
  );
}
export function definition(value: unknown): Definition {
  const data = object(value, "definition");
  return {
    name: nullableString(data.name, "definition.name"),
    hash: string(data.hash, "definition.hash"),
    items: hashes(data.items, "definition.items"),
  };
}
export function definitionView(value: unknown): DefinitionView {
  const data = object(value, "definition view");
  const entries = object(data.entries, "entries");
  const parsed: DefinitionView["entries"] = {};
  for (const name of Object.keys(entries)) {
    const effects = object(
      object(entries[name], name).effects,
      `${name}.effects`,
    );
    if (typeof effects.unknown !== "boolean")
      throw new Error(`${name}.effects.unknown: expected boolean`);
    parsed[name] = {
      effects: {
        labels: array(effects.labels, "labels").map((label) =>
          string(label, "label"),
        ),
        unknown: effects.unknown,
      },
    };
  }
  return {
    ...definition(value),
    source: string(data.source, "source"),
    def: json(object(data.def, "def"), "def") as Row,
    behavior_hash: string(data.behavior_hash, "behavior_hash"),
    ...(data.wasm_hash === undefined ? {} : { wasm_hash: string(data.wasm_hash, "wasm_hash") }),
    ...(data.toolchain_hash === undefined ? {} : { toolchain_hash: string(data.toolchain_hash, "toolchain_hash") }),
    entries: parsed,
  };
}
export function definitionDiff(value: unknown): DefinitionDiff {
  const data = object(value, "definition diff");
  const items = (value: unknown) =>
    array(value, "items").map((value) => {
      const item = object(value, "item");
      return {
        name: string(item.name, "name"),
        hash: string(item.hash, "hash"),
      };
    });
  return {
    old: string(data.old, "old"),
    new: string(data.new, "new"),
    added: items(data.added),
    removed: items(data.removed),
    changed: array(data.changed, "changed").map((value) => {
      const item = object(value, "change");
      return {
        name: string(item.name, "name"),
        old: string(item.old, "old"),
        new: string(item.new, "new"),
      };
    }),
  };
}
export function history(value: unknown): Revision[] {
  return array(value, "revisions").map((value) => {
    const data = object(value, "revision");
    return {
      name: string(data.name, "name"),
      hash: string(data.hash, "hash"),
      timestamp: integer(data.timestamp, "timestamp"),
      changes: data.changes === null ? null : definitionDiff(data.changes),
    };
  });
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
  if (depth > 128) throw new Error("supervision tree: depth exceeds 128");
  const data = object(value, "supervision tree");
  return {
    id: string(data.id, "supervision tree.id"),
    status: string(data.status, "supervision tree.status"),
    behavior_hash: string(data.behavior_hash, "supervision tree.behavior_hash"),
    cursor: integer(data.cursor, "supervision tree.cursor"),
    children: array(data.children, "supervision tree.children").map((value) =>
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
      throw new Error(`supervision tree: duplicate actor ${node.id}`);
    seen.add(node.id);
    const actor = byId.get(node.id);
    if (!actor) throw new Error(`actors list: missing tree actor ${node.id}`);
    result.push({ ...actor, depth });
    node.children.forEach((child) => visit(child, depth + 1));
  }
  visit(root, 0);
  return result;
}
export function validation(value: unknown): Validation {
  const data = object(value, "validation");
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

export interface UpdateSession {
  id: string;
  revision: number;
  target: string;
  status: "pending" | "complete" | "needs_repair" | "conflict" | "aborted";
  changes: { names: string[]; old_hash: string; new_hash: string }[];
  diagnostics: {
    hash: string;
    names: string[];
    source: string;
    diagnostics: Json[];
    build: Row | null;
  }[];
}
export function updateSession(value: unknown): UpdateSession {
  const data = object(object(value, "update result").update, "update");
  const status = string(data.status, "update.status");
  if (
    !["pending", "complete", "needs_repair", "conflict", "aborted"].includes(
      status,
    )
  )
    throw new Error(`update.status: unknown status ${status}`);
  const revision = integer(data.revision, "update.revision");
  if (revision < 0)
    throw new Error("update.revision: expected nonnegative integer");
  const names = (value: unknown) =>
    array(value, "names").map((value) => string(value, "name"));
  return {
    id: string(data.id, "update.id"),
    revision,
    target: string(data.target, "update.target"),
    status: status as UpdateSession["status"],
    changes: array(data.changes, "update.changes").map((value) => {
      const change = object(value, "update change");
      return {
        names: names(change.names),
        old_hash: string(change.old_hash, "old_hash"),
        new_hash: string(change.new_hash, "new_hash"),
      };
    }),
    diagnostics: array(data.diagnostics, "update.diagnostics").map((value) => {
      const diagnostic = object(value, "update diagnostic");
      return {
        hash: string(diagnostic.hash, "hash"),
        names: names(diagnostic.names),
        source: string(diagnostic.source, "source"),
        diagnostics: array(diagnostic.diagnostics, "compiler diagnostics").map(
          (value) => json(value, "compiler diagnostic"),
        ),
        build:
          diagnostic.build === null
            ? null
            : (json(object(diagnostic.build, "build"), "build") as Row),
      };
    }),
  };
}
export function repairFields(session: UpdateSession): Record<string, string> {
  return {
    id: session.id,
    revision: String(session.revision),
    changes: JSON.stringify(
      Object.fromEntries(
        session.diagnostics.map((item) => [
          item.names[0] ?? item.hash,
          { source: item.source },
        ]),
      ),
      null,
      2,
    ),
  };
}

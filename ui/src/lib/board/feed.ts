/** The board model and its one reducer: journal events and actor tree polls fold into `Model`. */

export interface JournalEvent {
  seq: number;
  /** Unix seconds; `definition_records.ts` is written with SQLite `unixepoch()`. */
  ts: number;
  event: Record<string, unknown>;
}
export interface Definition {
  hash: string;
  name: string | null;
  lang: string;
  exports: string[];
  /** `def.sig.effects.labels`; `call` draws the isolated edge, every other label the host edge. */
  effects: string[];
  /** Alias to dependency hash, from the `defined` event's `deps`. */
  deps: Record<string, string>;
  componentHash: string | null;
  seq: number;
}
export interface Build {
  componentHash: string;
  logsRef: string | null;
  ms: number | null;
  size: number | null;
  rustcInvocations: number | null;
  seq: number;
  ts: number;
  /** Merged `build_stages` objects from the CAS log, `null` until fetched. */
  stages: Record<string, number> | null;
  stagesError: string | null;
}
export interface FeedRow {
  seq: number;
  ts: number;
  type: string;
  event: Record<string, unknown>;
}
export interface Actor {
  id: string;
  /** `behavior_hash`, the definition hash the actor runs. */
  hash: string;
  status: string;
  cursor: number;
  parent: string | null;
  depth: number;
}
export interface Model {
  /** Highest journal seq folded in. */
  seq: number;
  definitions: Record<string, Definition>;
  /** Keyed by component hash; joined to definitions through `Definition.componentHash`. */
  builds: Record<string, Build>;
  /** Newest first, at most `FEED_CAP` rows. */
  feed: FeedRow[];
  /** Definition hash to the epoch millisecond its `call_completed` mark expires. */
  activeUntil: Record<string, number>;
  actors: Record<string, Actor>;
  /** Supervision tree preorder. */
  actorOrder: string[];
  /** Actor id to the epoch millisecond its cursor-change highlight expires. */
  bumpedUntil: Record<string, number>;
}

export const FEED_CAP = 500;
export const ACTIVE_MS = 1500;

export function empty(): Model {
  return {
    seq: 0,
    definitions: {},
    builds: {},
    feed: [],
    activeUntil: {},
    actors: {},
    actorOrder: [],
    bumpedUntil: {},
  };
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
function text(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}
function integer(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
function strings(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

/** The wire row of `/v1/events` and `/v1/stream`; anything else is a protocol error. */
export function parseJournalEvent(value: unknown): JournalEvent {
  const row = record(value);
  if (!Number.isSafeInteger(row.seq))
    throw new Error("journal event: seq must be a safe integer");
  if (typeof row.ts !== "number")
    throw new Error(`journal event ${String(row.seq)}: ts must be a number`);
  if (typeof row.event !== "object" || row.event === null)
    throw new Error(`journal event ${String(row.seq)}: event must be an object`);
  return {
    seq: row.seq as number,
    ts: row.ts,
    event: row.event as Record<string, unknown>,
  };
}

function clone(model: Model): Model {
  return {
    seq: model.seq,
    definitions: { ...model.definitions },
    builds: { ...model.builds },
    feed: [...model.feed],
    activeUntil: { ...model.activeUntil },
    actors: { ...model.actors },
    actorOrder: [...model.actorOrder],
    bumpedUntil: { ...model.bumpedUntil },
  };
}

function fold(draft: Model, item: JournalEvent, now: number) {
  // Journal seqs only grow; a replayed or reordered row is a duplicate and is dropped.
  if (item.seq <= draft.seq) return;
  draft.seq = item.seq;
  const event = item.event;
  const type = text(event.type) ?? "event";
  draft.feed.unshift({ seq: item.seq, ts: item.ts, type, event });
  if (draft.feed.length > FEED_CAP) draft.feed.length = FEED_CAP;

  if (type === "defined") {
    const def = record(event.def);
    const hash = text(def.hash);
    if (hash === null) return;
    const sig = record(def.sig);
    const previous = draft.definitions[hash];
    draft.definitions[hash] = {
      hash,
      name: text(event.name) ?? previous?.name ?? null,
      lang: text(def.lang) ?? "",
      exports: Array.isArray(sig.exports)
        ? sig.exports.flatMap((item) => {
            const name = text(record(item).name);
            return name === null ? [] : [name];
          })
        : [],
      effects: [...new Set(strings(record(sig.effects).labels))],
      deps: Object.fromEntries(
        Object.entries(record(event.deps)).flatMap(
          ([alias, target]): [string, string][] =>
            typeof target === "string" ? [[alias, target]] : [],
        ),
      ),
      componentHash: text(def.component_hash),
      seq: item.seq,
    };
  } else if (type === "component_built") {
    const componentHash = text(event.component_hash);
    if (componentHash === null) return;
    const previous = draft.builds[componentHash];
    draft.builds[componentHash] = {
      componentHash,
      logsRef: text(event.logs_ref),
      ms: integer(event.ms),
      size: integer(event.size),
      rustcInvocations: integer(event.rustc_invocations),
      seq: item.seq,
      ts: item.ts,
      stages: previous?.stages ?? null,
      stagesError: previous?.stagesError ?? null,
    };
  } else if (type === "call_completed") {
    const hash = text(event.definition_hash);
    if (hash !== null) draft.activeUntil[hash] = now + ACTIVE_MS;
  } else if (type === "actor_message") {
    const id = text(event.actor);
    const cursor = integer(event.cursor);
    if (id === null || cursor === null) return;
    const actor = draft.actors[id];
    if (actor) {
      if (actor.cursor !== cursor) {
        draft.actors[id] = { ...actor, cursor };
        draft.bumpedUntil[id] = now + ACTIVE_MS;
      }
    } else {
      // Provisional until the next tree poll reconciles parent, depth and status.
      draft.actors[id] = {
        id,
        hash: text(event.definition_hash) ?? "",
        status: "unknown",
        cursor,
        parent: null,
        depth: 0,
      };
      draft.actorOrder.push(id);
      draft.bumpedUntil[id] = now + ACTIVE_MS;
    }
  }
}

/** Fold journal events (any order of arrival) into a new model; unknown types become feed rows only. */
export function apply(
  model: Model,
  events: JournalEvent[],
  now: number = Date.now(),
): Model {
  const draft = clone(model);
  for (const item of events) fold(draft, item, now);
  return draft;
}

interface TreeNode {
  id: string;
  status: string;
  hash: string;
  cursor: number;
  children: TreeNode[];
}
function parseTree(value: unknown, depth: number): TreeNode {
  if (depth > 128) throw new Error("actor tree: depth exceeds 128");
  const node = record(value);
  const id = text(node.id);
  if (id === null) throw new Error("actor tree: node without id");
  const cursor = integer(node.cursor);
  if (cursor === null) throw new Error(`actor tree: ${id} has no cursor`);
  if (!Array.isArray(node.children))
    throw new Error(`actor tree: ${id} has no children array`);
  return {
    id,
    status: text(node.status) ?? "unknown",
    hash: text(node.behavior_hash) ?? "",
    cursor,
    children: node.children.map((child) => parseTree(child, depth + 1)),
  };
}

/** Replace the actor set with a `tree` command result; a changed cursor highlights the row. */
export function applyTree(
  model: Model,
  tree: unknown,
  now: number = Date.now(),
): Model {
  const root = parseTree(tree, 0);
  const draft = clone(model);
  draft.actors = {};
  draft.actorOrder = [];
  const visit = (node: TreeNode, parent: string | null, depth: number) => {
    if (draft.actors[node.id])
      throw new Error(`actor tree: duplicate actor ${node.id}`);
    const previous = model.actors[node.id];
    if (previous && previous.cursor !== node.cursor)
      draft.bumpedUntil[node.id] = now + ACTIVE_MS;
    draft.actors[node.id] = {
      id: node.id,
      hash: node.hash,
      status: node.status,
      cursor: node.cursor,
      parent,
      depth,
    };
    draft.actorOrder.push(node.id);
    for (const child of node.children) visit(child, node.id, depth + 1);
  };
  visit(root, null, 0);
  for (const id of Object.keys(draft.bumpedUntil))
    if (!draft.actors[id]) delete draft.bumpedUntil[id];
  return draft;
}

/** Record a fetched stage breakdown, or the fetch/parse failure, on one build. */
export function setStages(
  model: Model,
  componentHash: string,
  outcome: Record<string, number> | Error,
): Model {
  const build = model.builds[componentHash];
  if (!build) throw new Error(`build ${componentHash} is not in the model`);
  const draft = clone(model);
  draft.builds[componentHash] =
    outcome instanceof Error
      ? { ...build, stages: null, stagesError: outcome.message }
      : { ...build, stages: outcome, stagesError: null };
  return draft;
}

/** Drop expired highlights; returns the same model when nothing expired. */
export function prune(model: Model, now: number = Date.now()): Model {
  const active = Object.entries(model.activeUntil).filter(
    ([, until]) => until > now,
  );
  const bumped = Object.entries(model.bumpedUntil).filter(
    ([, until]) => until > now,
  );
  if (
    active.length === Object.keys(model.activeUntil).length &&
    bumped.length === Object.keys(model.bumpedUntil).length
  )
    return model;
  return {
    ...model,
    activeUntil: Object.fromEntries(active),
    bumpedUntil: Object.fromEntries(bumped),
  };
}

/** Earliest highlight expiry, or `null` when nothing is lit. */
export function nextExpiry(model: Model): number | null {
  const times = [
    ...Object.values(model.activeUntil),
    ...Object.values(model.bumpedUntil),
  ];
  return times.length ? Math.min(...times) : null;
}

export function definitionByComponent(
  model: Model,
  componentHash: string,
): Definition | undefined {
  return Object.values(model.definitions).find(
    (def) => def.componentHash === componentHash,
  );
}
export function buildOf(model: Model, def: Definition): Build | undefined {
  return def.componentHash === null ? undefined : model.builds[def.componentHash];
}
/** Definitions whose `deps` name `hash`, sorted by name then hash. */
export function dependentsOf(model: Model, hash: string): Definition[] {
  return Object.values(model.definitions)
    .filter((def) => def.hash !== hash && Object.values(def.deps).includes(hash))
    .sort((a, b) => {
      const ka = `${a.name ?? "￿"}\n${a.hash}`;
      const kb = `${b.name ?? "￿"}\n${b.hash}`;
      return ka < kb ? -1 : ka > kb ? 1 : 0;
    });
}

export interface Run {
  scope: string;
  seq: number;
  ts: number;
  completed: boolean;
  definitionHash: string | null;
  entry: string | null;
  outcome: string | null;
  elapsedMs: number | null;
  argsHash: string | null;
  traceHash: string | null;
}
/** The newest `call_completed` (or, failing that, `call_checkpoint`) row for a trace scope still held in the feed. */
export function runOf(model: Model, scope: string): Run | null {
  const row = model.feed.find(
    (row) =>
      (row.type === "call_completed" || row.type === "call_checkpoint") &&
      row.event.scope === scope,
  );
  if (!row) return null;
  const event = row.event;
  return {
    scope,
    seq: row.seq,
    ts: row.ts,
    completed: row.type === "call_completed",
    definitionHash: text(event.definition_hash),
    entry: text(event.entry),
    outcome: outcomeLabel(event.outcome),
    elapsedMs: integer(event.elapsed_ms),
    argsHash: text(event.args_hash),
    traceHash: text(event.trace_hash),
  };
}

/** `TraceOutcome` is `#[serde(tag = "status")]`: `success`, `error`, `cancelled`. */
export function outcomeLabel(outcome: unknown): string | null {
  if (outcome === null || outcome === undefined) return null;
  if (typeof outcome === "string") return outcome.toLowerCase();
  const status = text(record(outcome).status);
  if (status === null) return "unknown";
  return status === "success" ? "ok" : status;
}

export interface RowSummary {
  /** Definition hash the row is about, if any. */
  hash: string | null;
  name: string | null;
  outcome: string | null;
  elapsedMs: number | null;
  actor: string | null;
  cursor: number | null;
  /** Trace scope of a call row, if any. */
  scope: string | null;
}
/** What the feed shows for a row, resolved against the model's definitions. */
export function summarize(model: Model, row: FeedRow): RowSummary {
  const event = row.event;
  let hash: string | null = null;
  let name: string | null = null;
  if (row.type === "defined") {
    hash = text(record(event.def).hash);
    name = text(event.name);
  } else if (row.type === "component_built") {
    const component = text(event.component_hash);
    const def =
      component === null ? undefined : definitionByComponent(model, component);
    hash = def?.hash ?? component;
    name = def?.name ?? null;
  } else {
    hash = text(event.definition_hash) ?? text(event.def_hash);
  }
  if (name === null && hash !== null)
    name = model.definitions[hash]?.name ?? null;
  return {
    hash,
    name,
    outcome:
      row.type === "call_completed" || row.type === "call_checkpoint"
        ? outcomeLabel(event.outcome)
        : null,
    elapsedMs: row.type === "call_completed" ? integer(event.elapsed_ms) : null,
    actor: text(event.actor),
    cursor: integer(event.cursor),
    scope:
      row.type === "call_completed" || row.type === "call_checkpoint"
        ? text(event.scope)
        : null,
  };
}

export type RowTarget =
  | { kind: "def"; hash: string }
  | { kind: "actor"; id: string }
  | { kind: "build"; hash: string }
  | { kind: "run"; scope: string };
/** What clicking a feed row opens: the build, the run, the actor, or the definition it names. */
export function targetOfRow(model: Model, row: FeedRow): RowTarget | null {
  const summary = summarize(model, row);
  if (row.type === "component_built") {
    const component = text(row.event.component_hash);
    return component === null ? null : { kind: "build", hash: component };
  }
  if (summary.scope !== null) return { kind: "run", scope: summary.scope };
  if (row.type === "actor_message" && summary.actor !== null)
    return { kind: "actor", id: summary.actor };
  if (summary.hash !== null && model.definitions[summary.hash])
    return { kind: "def", hash: summary.hash };
  return null;
}

import { build, compatible, orderChildren, patch, preserveFocus, readTree, type OnEvent, type Tree } from "./patch";
import type { DeltaStream, Frame, SortValue, TreeRow } from "./stream";

interface Pending { messageKey: string }
export interface BoundRow {
  node: Element;
  tree: Tree;
  sort: SortValue;
  authoritative?: TreeRow;
  pending?: Pending;
}
export interface BindOptions { onEvent: OnEvent; onError?: (error: Error) => void }
export interface Binding {
  readonly rows: ReadonlyMap<string, BoundRow>;
  pending(key: string, tree: Tree, messageKey: string): void;
  receive(frame: Frame): void;
  destroy(): void;
}

function compare(left: SortValue, right: SortValue): number {
  if (left === right) return 0;
  if (Array.isArray(left) && Array.isArray(right)) {
    for (let index = 0; index < Math.min(left.length, right.length); index++) {
      const result = compare(left[index]!, right[index]!);
      if (result) return result;
    }
    return left.length - right.length;
  }
  if (typeof left === "number" && typeof right === "number") return left - right;
  if (typeof left === "string" && typeof right === "string") return left < right ? -1 : 1;
  const rank = (value: SortValue) => value === null ? 0 : typeof value === "boolean" ? 1 : typeof value === "number" ? 2 : typeof value === "string" ? 3 : 4;
  if (rank(left) !== rank(right)) return rank(left) - rank(right);
  return String(left) < String(right) ? -1 : 1;
}

export function bind(container: Element, stream: DeltaStream, options: BindOptions): Binding {
  // Entries leave on authoritative delete, replacement snapshot, failed new pending row, or destroy().
  const rows = new Map<string, BoundRow>();
  let source: string | undefined;
  // A snapshot replaces this cursor; accepted deltas advance it, destroy() releases it.
  let floor = -1;
  // Only a full snapshot leaves the initial/resnapshot barrier.
  let awaitingSnapshot = true;
  let destroyed = false;
  const cx = (key: string) => ({ key, onEvent: options.onEvent });
  function upsert(row: TreeRow, authoritative: boolean, forceValue = false) {
    let entry = rows.get(row.key);
    if (entry) {
      patch(entry.node, entry.tree, row.tree, { ...cx(row.key), forceValue });
      entry.tree = row.tree;
      entry.sort = row.sort;
    } else {
      const node = build(container.ownerDocument, row.tree, cx(row.key)) as Element;
      node.setAttribute("data-key", row.key);
      entry = { node, tree: row.tree, sort: row.sort };
      rows.set(row.key, entry);
    }
    if (authoritative) entry.authoritative = row;
  }
  function remove(key: string) {
    rows.get(key)?.node.remove();
    rows.delete(key);
  }
  function order() {
    const ordered = [...rows.entries()].sort(([leftKey, left], [rightKey, right]) =>
      compare(left.sort, right.sort) || (leftKey < rightKey ? -1 : leftKey === rightKey ? 0 : 1));
    orderChildren(container, ordered.map((entry) => entry[1].node));
  }
  function clearPending(entry: BoundRow) {
    // Matching delta verdict, dead letter, or destroy() leaves pending state.
    delete entry.pending;
    entry.node.removeAttribute("data-pending");
  }
  function preflight(next: TreeRow[]) {
    const trees = new Map([...rows.entries()].map(([key, row]) => [key, row.tree]));
    for (const row of next) {
      readTree(row.tree, `tree ${row.key}`);
      const previous = trees.get(row.key);
      if (previous) {
        try { compatible(previous, row.tree, true); }
        catch (error) { throw new Error(`tree ${row.key}: ${error}`); }
      }
      trees.set(row.key, row.tree);
    }
  }
  function apply(frame: Frame) {
    if (destroyed) throw new Error("binding is destroyed");
    if (frame.type === "dead_letter") {
      for (const [key, entry] of rows) {
        if (entry.pending?.messageKey !== frame.key) continue;
        if (entry.authoritative) {
          compatible(entry.tree, entry.authoritative.tree, true);
          upsert(entry.authoritative, true, true);
          clearPending(entry);
        } else remove(key);
      }
      order();
      options.onError?.(new Error(`message ${frame.key}: ${frame.error}`));
      return;
    }
    if (source !== undefined && frame.source !== source) throw new Error(`actor ${frame.source}: binding belongs to ${source}`);
    source = frame.source;
    if (frame.type === "resnapshot") { awaitingSnapshot = true; return; }
    if (frame.type === "snapshot") {
      const keys = new Set<string>();
      for (const row of frame.rows) {
        if (keys.has(row.key)) throw new Error(`actor ${source} seq ${frame.seq}: duplicate snapshot key ${row.key}`);
        keys.add(row.key);
      }
      preflight(frame.rows);
      for (const row of frame.rows) upsert(row, true);
      for (const key of rows.keys()) if (!keys.has(key)) remove(key);
      floor = frame.change_id;
      awaitingSnapshot = false;
      order();
      return;
    }
    if (awaitingSnapshot) throw new Error(`actor ${source} seq ${frame.seq}: delta while resnapshot flag is set`);
    let previousId = -1;
    for (const change of frame.rows) {
      if (change.change_id <= previousId) throw new Error(`actor ${source} seq ${frame.seq}: change_id is not increasing`);
      previousId = change.change_id;
    }
    const changes = frame.rows.filter((change) => change.change_id > floor);
    preflight(changes.flatMap((change) => change.after ? [change.after] : []));
    for (const change of changes) {
      if (change.change_type === -1) remove(change.before!.key);
      else upsert(change.after!, true);
      floor = change.change_id;
    }
    for (const [rowKey, entry] of rows) {
      const key = entry.pending?.messageKey;
      if (key !== undefined && (key === frame.key || key === frame.cause)) {
        if (entry.authoritative) {
          upsert(entry.authoritative, true);
          clearPending(entry);
        } else remove(rowKey);
      }
    }
    order();
  }
  function receive(frame: Frame) {
    try { preserveFocus(container, () => apply(frame)); }
    catch (error) {
      awaitingSnapshot = true;
      const failure = new Error(`actor ${source ?? ("source" in frame ? frame.source : "unknown")} seq ${"seq" in frame ? frame.seq : -1}: bind: ${error}`);
      if (options.onError) options.onError(failure);
      else throw failure;
    }
  }
  const unsubscribe = stream.subscribe(receive);
  return {
    rows,
    receive,
    pending(key, tree, messageKey) {
      if (destroyed) throw new Error("binding is destroyed");
      if (!messageKey) throw new Error(`tree ${key}: pending message key is empty`);
      const row = { key, tree: readTree(tree), sort: rows.get(key)?.sort ?? [key] };
      preflight([row]);
      preserveFocus(container, () => {
        upsert(row, false);
        const entry = rows.get(key)!;
        entry.pending = { messageKey };
        entry.node.setAttribute("data-pending", messageKey);
        order();
      });
    },
    destroy() {
      unsubscribe();
      destroyed = true;
      for (const key of rows.keys()) remove(key);
    },
  };
}

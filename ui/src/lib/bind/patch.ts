export type Attribute = string | number | boolean | null;
export interface Tree {
  tag: string;
  key?: string;
  attrs?: Record<string, Attribute>;
  children?: Array<Tree | string>;
}
export interface EventPayload {
  type: string;
  value?: string;
  checked?: boolean;
}
export type OnEvent = (key: string, name: string, payload: EventPayload) => void;
export interface PatchContext {
  key: string;
  onEvent: OnEvent;
  forceValue?: boolean;
}
interface WiredEvent {
  name: string;
  listener: EventListener;
}
// Weak keys release listeners with their DOM nodes; setAttribute removes changed handlers.
const listeners = new WeakMap<Element, Map<string, WiredEvent>>();

export function readTree(value: unknown, context = "tree"): Tree {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error(`${context}: expected tree object`);
  const row = value as Record<string, unknown>;
  if (typeof row.tag !== "string" || !/^[a-z][a-z0-9-]*$/.test(row.tag))
    throw new Error(`${context}: invalid tag`);
  if (["script", "iframe", "object", "embed", "base", "meta", "link"].includes(row.tag))
    throw new Error(`${context}: active tag ${row.tag} is forbidden`);
  if (row.key !== undefined && typeof row.key !== "string")
    throw new Error(`${context}: key must be a string`);
  const tree: Tree = { tag: row.tag };
  if (typeof row.key === "string") tree.key = row.key;
  if (row.attrs !== undefined) {
    if (!row.attrs || typeof row.attrs !== "object" || Array.isArray(row.attrs))
      throw new Error(`${context}: attrs must be an object`);
    tree.attrs = {};
    for (const [name, attr] of Object.entries(row.attrs)) {
      if (!/^[a-zA-Z_][a-zA-Z0-9_:.-]*$/.test(name))
        throw new Error(`${context}: invalid attribute ${name}`);
      if (attr !== null && !["string", "number", "boolean"].includes(typeof attr))
        throw new Error(`${context}: attribute ${name} must be a scalar`);
      if (name.toLowerCase().startsWith("on") && typeof attr !== "string")
        throw new Error(`${context}: event ${name} must name a message`);
      if (name.toLowerCase() === "srcdoc" || (typeof attr === "string" && /^\s*(javascript|vbscript):/i.test(attr)))
        throw new Error(`${context}: active attribute ${name} is forbidden`);
      tree.attrs[name] = attr as Attribute;
    }
  }
  if (row.children !== undefined) {
    if (!Array.isArray(row.children)) throw new Error(`${context}: children must be an array`);
    const keys = new Set<string>();
    tree.children = row.children.map((child, index) => {
      if (typeof child === "string") return child;
      const parsed = readTree(child, `${context}.children[${index}]`);
      if (parsed.tag === "li" && parsed.key === undefined)
        throw new Error(`${context}: list item requires a key`);
      if (parsed.key !== undefined) {
        if (keys.has(parsed.key)) throw new Error(`${context}: duplicate child key ${parsed.key}`);
        keys.add(parsed.key);
      }
      return parsed;
    });
  }
  return tree;
}

function attribute(node: Element, name: string, value: Attribute | undefined, cx: PatchContext) {
  if (name.toLowerCase().startsWith("on")) {
    const type = name.slice(2).toLowerCase();
    let wired = listeners.get(node);
    const previous = wired?.get(name);
    if (previous?.name === value) return;
    if (previous) {
      node.removeEventListener(type, previous.listener);
      wired!.delete(name);
    }
    if (typeof value !== "string") return;
    const listener: EventListener = (event) => {
      const payload: EventPayload = { type: event.type };
      if ("value" in node && typeof node.value === "string") payload.value = node.value;
      if ("checked" in node && typeof node.checked === "boolean") payload.checked = node.checked;
      cx.onEvent(cx.key, value, payload);
    };
    if (!wired) { wired = new Map(); listeners.set(node, wired); }
    wired.set(name, { name: value, listener });
    node.addEventListener(type, listener);
    return;
  }
  if (name === "value" && "value" in node) {
    const next = value == null ? "" : String(value);
    // The focused editor owns its draft; authoritative text applies after focus leaves.
    if ((cx.forceValue || node.ownerDocument.activeElement !== node) && node.value !== next) node.value = next;
    return;
  }
  if (name === "checked" && "checked" in node) node.checked = value === true;
  if (value === undefined || value === null || value === false) {
    if (node.hasAttribute(name)) node.removeAttribute(name);
  } else {
    const text = value === true ? "" : String(value);
    if (node.getAttribute(name) !== text) node.setAttribute(name, text);
  }
}

export function build(document: Document, tree: Tree | string, cx: PatchContext): Node {
  if (typeof tree === "string") return document.createTextNode(tree);
  const node = document.createElement(tree.tag);
  for (const [name, value] of Object.entries(tree.attrs ?? {})) attribute(node, name, value, cx);
  for (const child of tree.children ?? []) node.appendChild(build(document, child, cx));
  return node;
}

function childKey(tree: Tree | string | undefined): string | undefined {
  return typeof tree === "object" ? tree.key : undefined;
}

interface ChildMatch {
  index: number;
  next: Tree | string;
  previousIndex?: number;
  previous?: Tree | string;
}
interface TagQueue { indices: number[]; cursor: number }

/** Keys own identity; unkeyed elements consume their tag's document-order queue. */
function matchChildren(previous: Array<Tree | string>, next: Array<Tree | string>): ChildMatch[] {
  const keyed = new Map<string, number>();
  const tags = new Map<string, TagQueue>();
  previous.forEach((child, index) => {
    if (typeof child === "string") return;
    if (child.key !== undefined) { keyed.set(child.key, index); return; }
    let queue = tags.get(child.tag);
    if (!queue) { queue = { indices: [], cursor: 0 }; tags.set(child.tag, queue); }
    queue.indices.push(index);
  });
  const used = new Set<number>();
  return next.map((child, index) => {
    let previousIndex: number | undefined;
    if (typeof child === "string") {
      if (typeof previous[index] === "string") previousIndex = index;
    } else if (child.key !== undefined) previousIndex = keyed.get(child.key);
    else {
      const queue = tags.get(child.tag);
      if (queue) previousIndex = queue.indices[queue.cursor++];
    }
    if (previousIndex === undefined || used.has(previousIndex)) return { index, next: child };
    used.add(previousIndex);
    return { index, next: child, previousIndex, previous: previous[previousIndex] };
  });
}

/** Fail before mutating if a living keyed node would need a different Element class. */
export function compatible(oldTree: Tree | string, next: Tree | string, identity = false): void {
  if (identity && (typeof oldTree === "string" || typeof next === "string" || oldTree.tag !== next.tag))
    throw new Error(`tree key ${typeof oldTree === "object" ? oldTree.key ?? "row" : "text"}: tag change would replace a live node`);
  if (typeof oldTree === "string" || typeof next === "string" || oldTree.tag !== next.tag) return;
  for (const match of matchChildren(oldTree.children ?? [], next.children ?? [])) {
    if (match.previous !== undefined)
      compatible(match.previous, match.next, childKey(match.next) !== undefined);
  }
}

function move(parent: Node, node: Node, before: Node | null) {
  if (node === before || (node.parentNode === parent && node.nextSibling === before)) return;
  parent.insertBefore(node, before);
}

/** Run after removals. The focused child (including a containing subtree) stays attached. */
export function orderChildren(parent: Node, desired: readonly Node[]) {
  const active = parent.ownerDocument?.activeElement;
  const anchor = active ? desired.findIndex((node) => node === active || node.contains(active)) : -1;
  let before: Node | null = null;
  for (let index = desired.length - 1; index > anchor; index--) {
    const node = desired[index]!;
    move(parent, node, before);
    before = node;
  }
  if (anchor < 0) return;
  before = desired[anchor]!;
  for (let index = anchor - 1; index >= 0; index--) {
    const node = desired[index]!;
    move(parent, node, before);
    before = node;
  }
}

export function patch(node: Node, previous: Tree | string, next: Tree | string, cx: PatchContext): Node {
  if (typeof previous === "string" && typeof next === "string") {
    if (node.nodeValue !== next) node.nodeValue = next;
    return node;
  }
  if (typeof previous === "string" || typeof next === "string" || previous.tag !== next.tag) {
    const replacement = build(node.ownerDocument!, next, cx);
    node.parentNode!.replaceChild(replacement, node);
    return replacement;
  }
  const element = node as Element;
  const attrs = next.attrs ?? {};
  for (const name of Object.keys(previous.attrs ?? {}))
    if (!(name in attrs)) attribute(element, name, undefined, cx);
  for (const [name, value] of Object.entries(attrs)) attribute(element, name, value, cx);
  const nodes = Array.from(element.childNodes);
  const used = new Set<number>();
  const ordered: Node[] = [];
  for (const match of matchChildren(previous.children ?? [], next.children ?? [])) {
    const oldNode = match.previousIndex === undefined ? undefined : nodes[match.previousIndex];
    let desired: Node;
    if (match.previousIndex !== undefined && oldNode && match.previous !== undefined) {
      used.add(match.previousIndex);
      desired = patch(oldNode, match.previous, match.next, cx);
    } else desired = build(element.ownerDocument, match.next, cx);
    ordered.push(desired);
  }
  nodes.forEach((oldNode, index) => { if (!used.has(index) && oldNode.parentNode === element) element.removeChild(oldNode); });
  orderChildren(element, ordered);
  return element;
}

/** Preserve selection and scroll without refocusing: ordering must never blur a surviving editor. */
export function preserveFocus(container: Element, action: () => void) {
  const active = container.ownerDocument.activeElement as HTMLElement | null;
  const contained = active !== null && container.contains(active);
  const input = active as HTMLInputElement | null;
  const start = contained && active && "selectionStart" in active ? input!.selectionStart : null;
  const end = contained && active && "selectionEnd" in active ? input!.selectionEnd : null;
  const direction = contained && active && "selectionDirection" in active ? input!.selectionDirection : null;
  const scroll: Array<{ node: Element; top: number; left: number }> = [];
  if (contained) for (let node: Element | null = active; node; node = node.parentElement)
    scroll.push({ node, top: node.scrollTop, left: node.scrollLeft });
  try { action(); } finally {
    if (contained && active && active.isConnected && container.contains(active)) {
      if (start !== null && end !== null && input?.setSelectionRange)
        input.setSelectionRange(start, end, direction ?? undefined);
      for (const item of scroll) { item.node.scrollTop = item.top; item.node.scrollLeft = item.left; }
    }
  }
}

import { Window as HappyWindow } from "happy-dom";
import { bind } from "../../src/lib/bind/bind";
import { parseFrame, type DeltaStream, type Frame } from "../../src/lib/bind/stream";
import type { Tree, EventPayload } from "../../src/lib/bind/patch";

export class TestStream implements DeltaStream {
  private receivers = new Set<(frame: Frame) => void>();
  subscribe(receive: (frame: Frame) => void) {
    this.receivers.add(receive);
    return () => { this.receivers.delete(receive); };
  }
  emit(wire: unknown) { const frame = parseFrame(wire); for (const receive of this.receivers) receive(frame); }
}
export interface WireRow { key: string; sort: number[]; tree: number[] }
export function row(key: string, order: number, tree: Tree): WireRow {
  const bytes = (value: unknown) => Array.from(new TextEncoder().encode(JSON.stringify(value)));
  return { key, sort: bytes([order]), tree: bytes(tree) };
}
export function tree(text: string, attrs: Tree["attrs"] = {}): Tree {
  return { tag: "div", attrs, children: [text] };
}
export function snapshot(rows: WireRow[], change_id = 0) {
  return { type: "snapshot", source: "view-1", seq: 0, key: "control", table: "tree", change_id,
    rows: rows.map((after, index) => ({ id: index + 1, after })) };
}
export function change(change_id: number, after: WireRow | null, before: WireRow | null = null) {
  return { change_id, change_type: after === null ? -1 : before === null ? 1 : 0, table: "tree", id: change_id,
    before, after, updates: null };
}
export function delta(rows: ReturnType<typeof change>[], key = "control", cause: string | null = null) {
  return { type: "delta", source: "view-1", seq: rows.at(-1)?.change_id ?? 1, key, cause, rows };
}
export function fixture() {
  // Existing tests have no DOM shim. Each new test owns a Window, closed by dispose().
  const window = new HappyWindow();
  const document = window.document as unknown as Document;
  const container = document.createElement("section");
  document.body.appendChild(container);
  const stream = new TestStream();
  const errors: string[] = [];
  const events: Array<{ key: string; name: string; payload: EventPayload }> = [];
  const binding = bind(container, stream, {
    onEvent: (key, name, payload) => { events.push({ key, name, payload }); },
    onError: (error) => { errors.push(error.message); },
  });
  return { window, document, container, stream, binding, errors, events,
    dispose() { binding.destroy(); window.close(); } };
}

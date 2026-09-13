import { expect, test } from "bun:test";
import { delta, fixture, row, snapshot, tree } from "./fixture";

test("resnapshot_rebuilds_without_losing_focused_row", () => {
  const f = fixture();
  try {
    const a = row("a", 1, { tag: "div", children: [{ tag: "input", key: "editor", attrs: { value: "editable" } }] });
    const b = row("b", 2, tree("remove"));
    f.stream.emit(snapshot([a, b]));
    const node = f.binding.rows.get("a")!.node;
    const removed = f.binding.rows.get("b")!.node;
    const input = node.querySelector("input")!;
    input.focus();
    input.setSelectionRange(2, 4);
    f.stream.emit({ type: "resnapshot", source: "view-1" });
    expect(f.container.contains(node)).toBe(true);
    f.stream.emit(delta([], "stale"));
    expect(f.errors.at(-1)).toContain("resnapshot flag");
    f.stream.emit(snapshot([row("c", 0, tree("insert")), { ...a, sort: row("a", 3, tree("unused")).sort }], 20));
    expect(f.binding.rows.get("a")!.node).toBe(node);
    expect(node.querySelector("input")).toBe(input);
    expect(f.document.activeElement).toBe(input);
    expect(input.selectionStart).toBe(2);
    expect(input.selectionEnd).toBe(4);
    expect(removed.isConnected).toBe(false);
    expect(Array.from(f.container.children, (child) => child.getAttribute("data-key"))).toEqual(["c", "a"]);
    expect(f.errors).toHaveLength(1);
  } finally { f.dispose(); }
});

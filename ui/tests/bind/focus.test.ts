import { expect, test } from "bun:test";
import { change, delta, fixture, row, snapshot } from "./fixture";
import type { Tree } from "../../src/lib/bind/patch";

test("focus_and_caret_survive_promote", () => {
  const f = fixture();
  const render = (promoted: boolean): Tree => ({ tag: "div", attrs: { class: promoted ? "v2" : "v1" }, children: [
    { tag: "input", key: "editor", attrs: { value: "original", onInput: "rename", placeholder: promoted ? "New label" : "Label" } },
    { tag: "span", key: "count", children: [promoted ? "new template" : "old template"] },
  ] });
  try {
    const a = row("a", 1, render(false));
    const b = row("b", 2, render(false));
    f.stream.emit(snapshot([a, b]));
    const rowNode = f.binding.rows.get("a")!.node;
    const input = rowNode.querySelector("input")!;
    input.focus();
    input.value = "a local draft";
    input.setSelectionRange(3, 6, "backward");
    f.stream.emit(delta([change(1, row("a", 1, render(true)), a), change(2, row("b", 2, render(true)), b)], "control"));
    expect(f.binding.rows.get("a")!.node).toBe(rowNode);
    expect(rowNode.querySelector("input")).toBe(input);
    expect(f.document.activeElement).toBe(input);
    expect(input.value).toBe("a local draft");
    expect(input.selectionStart).toBe(3);
    expect(input.selectionEnd).toBe(6);
    expect(input.selectionDirection).toBe("backward");
    input.dispatchEvent(new f.window.Event("input") as unknown as Event);
    expect(f.events[0]).toEqual({ key: "a", name: "rename", payload: { type: "input", value: "a local draft", checked: false } });
    expect(f.errors).toEqual([]);
  } finally { f.dispose(); }
});

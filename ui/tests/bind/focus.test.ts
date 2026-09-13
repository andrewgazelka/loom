import { expect, test } from "bun:test";
import { change, delta, fixture, row, snapshot } from "./fixture";
import type { Tree } from "../../src/lib/bind/patch";

test("focus_and_caret_survive_promote", () => {
  const f = fixture();
  const render = (promoted: boolean): Tree => {
    const input: Tree = { tag: "input", attrs: { value: "original", onInput: "rename", placeholder: promoted ? "New label" : "Label" } };
    const first: Tree = { tag: "button", children: [promoted ? "first updated" : "first"] };
    const second: Tree = { tag: "button", children: [promoted ? "second updated" : "second"] };
    const keyed: Tree = { tag: "button", key: "fixed", children: ["keyed"] };
    return { tag: "div", attrs: { class: promoted ? "v2" : "v1" }, children: promoted
      ? [first, second, input, keyed, { tag: "em", children: ["new"] }, "new tail"]
      : [{ tag: "span", key: "leading", children: ["remove me"] }, input, first, keyed, second, "old tail"] };
  };
  try {
    const a = row("a", 1, render(false));
    const b = row("b", 2, render(false));
    f.stream.emit(snapshot([a, b]));
    const rowNode = f.binding.rows.get("a")!.node;
    const input = rowNode.querySelector("input")!;
    const leading = rowNode.firstChild!;
    const first = rowNode.childNodes[2]!;
    const keyed = rowNode.childNodes[3]!;
    const second = rowNode.childNodes[4]!;
    const tail = rowNode.lastChild!;
    const otherInput = f.binding.rows.get("b")!.node.querySelector("input")!;
    input.focus();
    input.value = "a local draft";
    input.setSelectionRange(3, 6, "backward");
    const active = f.document.activeElement;
    expect(rowNode.childNodes[1]).toBe(input);
    let blurs = 0;
    input.addEventListener("blur", () => { blurs++; });
    const mutations = new f.window.MutationObserver(() => {});
    mutations.observe(f.window.document.body, { childList: true, subtree: true });
    // Move the row as well as the input's child index; neither focused subtree may be detached.
    f.stream.emit(delta([change(1, row("a", 3, render(true)), a), change(2, row("b", 2, render(true)), b)], "control"));
    expect(f.document.activeElement).toBe(active);
    expect(blurs).toBe(0);
    for (const mutation of mutations.takeRecords()) {
      for (const removed of Array.from(mutation.removedNodes)) {
        expect(removed as unknown as Node).not.toBe(input);
        expect(removed as unknown as Node).not.toBe(rowNode);
      }
    }
    mutations.disconnect();
    expect(f.binding.rows.get("a")!.node).toBe(rowNode);
    expect(rowNode.querySelector("input")).toBe(input);
    expect(rowNode.childNodes[2]).toBe(input);
    expect(f.container.children[1]).toBe(rowNode);
    expect(leading.isConnected).toBe(false);
    expect(rowNode.childNodes[0]).toBe(first);
    expect(rowNode.childNodes[1]).toBe(second);
    expect(rowNode.childNodes[3]).toBe(keyed);
    expect(first.textContent).toBe("first updated");
    expect(second.textContent).toBe("second updated");
    expect(rowNode.lastChild).toBe(tail);
    expect(tail.nodeValue).toBe("new tail");
    expect(f.binding.rows.get("b")!.node.querySelector("input")).toBe(otherInput);
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

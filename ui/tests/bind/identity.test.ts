import { expect, test } from "bun:test";
import { change, delta, fixture, row, snapshot, tree } from "./fixture";

test("insert_update_delete_reorder_keep_node_identity", () => {
  const f = fixture();
  try {
    const first = row("a", 1, { tag: "div", children: [
      { tag: "button", key: "action", attrs: { onClick: "bump" }, children: ["+"] },
      { tag: "span", key: "label", children: ["before"] },
    ] });
    const second = row("b", 2, tree("second"));
    f.stream.emit(snapshot([second, first]));
    const node = f.binding.rows.get("a")!.node;
    const button = node.querySelector("button")!;
    const span = node.querySelector("span")!;
    const next = row("a", 3, { tag: "div", children: [
      { tag: "span", key: "label", children: ["after"] },
      { tag: "button", key: "action", attrs: { onClick: "increment" }, children: ["++"] },
    ] });
    f.stream.emit(delta([change(1, next, first)]));
    expect(f.binding.rows.get("a")!.node).toBe(node);
    expect(node.querySelector("button")).toBe(button);
    expect(node.querySelector("span")).toBe(span);
    expect(Array.from(f.container.children, (child) => child.getAttribute("data-key"))).toEqual(["b", "a"]);
    button.dispatchEvent(new f.window.Event("click") as unknown as Event);
    expect(f.events).toEqual([{ key: "a", name: "increment", payload: { type: "click", value: "" } }]);
    f.stream.emit(delta([change(2, row("c", 0, tree("new")))]));
    expect(f.container.firstElementChild?.getAttribute("data-key")).toBe("c");
    expect(f.binding.rows.get("a")!.node).toBe(node);
    f.stream.emit(delta([change(3, null, next)]));
    expect(f.binding.rows.has("a")).toBe(false);
    expect(node.isConnected).toBe(false);
    expect(f.errors).toEqual([]);
    const live = f.binding.rows.get("c")!.node;
    f.stream.emit(delta([change(4, row("c", 0, { tag: "span", children: ["invalid replacement"] }))]));
    expect(f.errors.at(-1)).toContain("tag change would replace a live node");
    expect(f.binding.rows.get("c")!.node).toBe(live);
    expect(live.textContent).toBe("new");
  } finally { f.dispose(); }
});

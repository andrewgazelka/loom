import { expect, test } from "bun:test";
import { change, delta, fixture, row, snapshot, tree } from "./fixture";

test("pending_is_cleared_by_matching_key_and_reverted_by_dead_letter", () => {
  const f = fixture();
  try {
    const initial = row("a", 1, tree("authoritative"));
    f.stream.emit(snapshot([initial]));
    const node = f.binding.rows.get("a")!.node;
    f.binding.pending("a", tree("optimistic"), "browser:1");
    expect(node.getAttribute("data-pending")).toBe("browser:1");
    f.stream.emit(delta([], "unrelated"));
    expect(node.hasAttribute("data-pending")).toBe(true);
    const accepted = row("a", 1, tree("accepted"));
    f.stream.emit(delta([change(1, accepted, initial)], "delta:source:1:view", "browser:1"));
    expect(node.textContent).toBe("accepted");
    expect(node.hasAttribute("data-pending")).toBe(false);
    f.binding.pending("a", tree("wrong"), "browser:2");
    f.stream.emit({ type: "dead_letter", key: "unrelated", error: "unrelated trap" });
    expect(node.textContent).toBe("wrong");
    f.stream.emit({ type: "dead_letter", key: "browser:2", error: "actor source seq 2: denied" });
    expect(f.binding.rows.get("a")!.node).toBe(node);
    expect(node.textContent).toBe("accepted");
    expect(node.hasAttribute("data-pending")).toBe(false);
    expect(f.errors.at(-1)).toContain("actor source seq 2: denied");
    f.binding.pending("new", tree("new guess"), "browser:3");
    f.stream.emit({ type: "dead_letter", key: "browser:3", error: "no insert" });
    expect(f.binding.rows.has("new")).toBe(false);
    f.binding.pending("a", tree("unused guess"), "browser:4");
    f.stream.emit(delta([], "browser:4"));
    expect(node.hasAttribute("data-pending")).toBe(false);
    expect(node.textContent).toBe("accepted");
  } finally { f.dispose(); }
});

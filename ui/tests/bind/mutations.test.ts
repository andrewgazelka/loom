import { expect, test } from "bun:test";
import { change, delta, fixture, row, snapshot, tree } from "./fixture";

test("patch_touches_only_changed_attrs_and_text", () => {
  const f = fixture();
  const observer = new f.window.MutationObserver(() => {});
  try {
    const before = row("a", 1, tree("before", { class: "same", title: "old", "data-old": "remove" }));
    f.stream.emit(snapshot([before]));
    observer.observe(f.window.document.body, { subtree: true, attributes: true, characterData: true, childList: true });
    const after = row("a", 1, tree("after", { class: "same", title: "new" }));
    f.stream.emit(delta([change(1, after, before)]));
    expect(observer.takeRecords().map((record) => ({ type: record.type, name: record.attributeName })).sort((a, b) =>
      `${a.type}:${a.name}`.localeCompare(`${b.type}:${b.name}`))).toEqual([
      { type: "attributes", name: "data-old" }, { type: "attributes", name: "title" },
      { type: "characterData", name: null },
    ]);
    f.stream.emit(delta([change(2, after, after)]));
    expect(observer.takeRecords()).toHaveLength(0);
    expect(f.errors).toEqual([]);
  } finally { observer.disconnect(); f.dispose(); }
});

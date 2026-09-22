import { describe, expect, test } from "bun:test";
import {
  OWN_FILE,
  groupFunctions,
  groupOf,
  indexLines,
  isOwnFunction,
  parseWasmModule,
  sourceOf,
  watLinesFor,
  type WasmLine,
} from "../../src/lib/board/wasm";

const wire = {
  debug: true,
  wat: Array.from({ length: 60 }, (_, index) => `  ;; line ${index + 1}`).join("\n"),
  functions: [
    { index: 0, name: "cabi_realloc", exported: true, start_line: 10, end_line: 20 },
    { index: 1, name: "loom_definition::counter::h1a2b3c", exported: false, start_line: 21, end_line: 45 },
    { index: 2, name: "core::fmt::write::h9f", exported: false, start_line: 46, end_line: 55 },
    { index: 3, name: null, exported: false, start_line: 56, end_line: 58 },
  ],
  lines: [
    { wat_line: 40, file: "src/lib.rs", line: 12 },
    { wat_line: 41, file: "src/lib.rs", line: 12 },
    { wat_line: 42, file: "src/lib.rs", line: 12 },
    { wat_line: 43, file: "src/lib.rs", line: 12 },
    { wat_line: 44, file: "src/lib.rs", line: 13 },
    { wat_line: 47, file: "/rustc/abc/library/core/src/fmt/mod.rs", line: 12 },
  ],
};

describe("wasm module parsing", () => {
  test("accepts the wire shape and renames fields", () => {
    const module = parseWasmModule(wire);
    expect(module.debug).toBe(true);
    expect(module.functions[1]).toEqual({
      index: 1,
      name: "loom_definition::counter::h1a2b3c",
      exported: false,
      startLine: 21,
      endLine: 45,
    });
    expect(module.lines[0]).toEqual({ watLine: 40, file: "src/lib.rs", line: 12 });
  });
  test("rejects an envelope, a missing wat, and an empty function range by name", () => {
    expect(() => parseWasmModule({ ok: true, result: wire })).toThrow("command envelope");
    expect(() => parseWasmModule({ ...wire, wat: 7 })).toThrow("wasm.wat: expected a string");
    expect(() => parseWasmModule({ ...wire, debug: "no" })).toThrow("wasm.debug");
    expect(() =>
      parseWasmModule({
        ...wire,
        functions: [{ index: 0, name: null, exported: false, start_line: 5, end_line: 4 }],
      }),
    ).toThrow("wasm.functions[0]: line range 5..4 is empty");
  });
});

describe("source to wat hot marking", () => {
  const index = indexLines(parseWasmModule(wire).lines);
  test("source line 12 marks wat lines 40 to 43 and nothing from another file", () => {
    expect(watLinesFor(index, 12)).toEqual([40, 41, 42, 43]);
    expect(watLinesFor(index, 13)).toEqual([44]);
    expect(watLinesFor(index, 12, "/rustc/abc/library/core/src/fmt/mod.rs")).toEqual([47]);
    expect(watLinesFor(index, 99)).toEqual([]);
  });
  test("a wat line maps back to its source position; unmapped lines give null", () => {
    expect(sourceOf(index, 41)).toEqual({ file: OWN_FILE, line: 12 });
    expect(sourceOf(index, 47)).toEqual({ file: "/rustc/abc/library/core/src/fmt/mod.rs", line: 12 });
    expect(sourceOf(index, 1)).toBeNull();
  });
  test("wat lines are returned ascending whatever the wire order", () => {
    const shuffled: WasmLine[] = [
      { watLine: 43, file: OWN_FILE, line: 12 },
      { watLine: 40, file: OWN_FILE, line: 12 },
      { watLine: 42, file: OWN_FILE, line: 12 },
    ];
    expect(watLinesFor(indexLines(shuffled), 12)).toEqual([40, 42, 43]);
  });
});

describe("function collapse classification", () => {
  const exports = ["counter"];
  test("own: the crate prefix or an export name in the symbol", () => {
    expect(isOwnFunction({ name: "loom_definition::counter::h1a2b3c" }, [])).toBe(true);
    expect(isOwnFunction({ name: "counter" }, exports)).toBe(true);
    expect(isOwnFunction({ name: "loom_definition_0123456789abcdef::helper::h00" }, [])).toBe(true);
  });
  test("external: std, core, alloc, the SDK, exported ABI shims and unnamed functions", () => {
    expect(isOwnFunction({ name: "core::fmt::write::h9f" }, exports)).toBe(false);
    expect(isOwnFunction({ name: "alloc::raw_vec::finish_grow::h1" }, exports)).toBe(false);
    expect(isOwnFunction({ name: "loom_guest_rs::effects::sleep::h2" }, exports)).toBe(false);
    expect(isOwnFunction({ name: "cabi_realloc" }, exports)).toBe(false);
    expect(isOwnFunction({ name: null }, exports)).toBe(false);
    expect(isOwnFunction({ name: "counter" }, [""])).toBe(false);
  });
  test("groups cover every wat line, label the gaps module, and expand only own functions", () => {
    const module = parseWasmModule(wire);
    const groups = groupFunctions(module.functions, exports, 60);
    expect(groups.map((group) => [group.label, group.start, group.end, group.own])).toEqual([
      ["module", 1, 9, false],
      ["cabi_realloc", 10, 20, false],
      ["loom_definition::counter::h1a2b3c", 21, 45, true],
      ["core::fmt::write::h9f", 46, 55, false],
      ["func #3", 56, 58, false],
      ["module", 59, 60, false],
    ]);
    expect(groupOf(groups, 41)?.key).toBe("fn:1");
    expect(groupOf(groups, 61)).toBeNull();
  });
  test("overlapping functions are a server error, not a double drawing", () => {
    const module = parseWasmModule(wire);
    expect(() =>
      groupFunctions(
        [...module.functions, { index: 9, name: "x", exported: false, startLine: 30, endLine: 31 }],
        exports,
        60,
      ),
    ).toThrow("function 9 starts at line 30, inside the previous function");
  });
});

import { describe, expect, test } from "bun:test";
import {
  formatSelection,
  parseSelection,
  sameSelection,
  type Selection,
} from "../../src/lib/board/selection";

const HASH = "a".repeat(64);

describe("board selection fragment", () => {
  test("parses each selection kind and ignores the token key", () => {
    expect(parseSelection(`#def=${HASH}`)).toEqual({ kind: "def", hash: HASH });
    expect(parseSelection(`#build=${HASH}`)).toEqual({ kind: "build", hash: HASH });
    expect(parseSelection("#actor=a0-counter")).toEqual({ kind: "actor", id: "a0-counter" });
    expect(parseSelection("#run=call%3Afixture")).toEqual({ kind: "run", scope: "call:fixture" });
    expect(parseSelection(`#token=secret&def=${HASH}`)).toEqual({ kind: "def", hash: HASH });
  });
  test("no fragment or a fragment without a selection key is the Overview", () => {
    expect(parseSelection("")).toBeNull();
    expect(parseSelection("#")).toBeNull();
    expect(parseSelection("#token=secret")).toBeNull();
  });
  test("round trips through formatSelection, including scopes that need encoding", () => {
    const selections: Selection[] = [
      { kind: "def", hash: HASH },
      { kind: "build", hash: HASH },
      { kind: "actor", id: "a0-counter" },
      { kind: "run", scope: "call:2026-09-21T10:00:00Z/1 2" },
    ];
    for (const selection of selections)
      expect(parseSelection(formatSelection(selection))).toEqual(selection);
    expect(formatSelection(null)).toBe("");
    expect(sameSelection({ kind: "def", hash: HASH }, { kind: "def", hash: HASH })).toBe(true);
    expect(sameSelection({ kind: "def", hash: HASH }, null)).toBe(false);
  });
  test("bad links fail by name instead of opening the Overview", () => {
    expect(() => parseSelection(`#def=${HASH}&actor=a0`)).toThrow("one of def, actor, build, run");
    expect(() => parseSelection("#def=abc")).toThrow("64-hex definition hash");
    expect(() => parseSelection("#build=abc")).toThrow("64-hex component hash");
    expect(() => parseSelection("#actor=")).toThrow("actor is empty");
    expect(() => parseSelection("#run=a&run=b")).toThrow("given 2 times");
  });
});

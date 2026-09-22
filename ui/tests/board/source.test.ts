import { describe, expect, test } from "bun:test";
import { displayedSource, splitLines } from "../../src/lib/board/source";

const stored = "pub fn counter(value:i64)->i64{value+1}\n";
const formatted = "pub fn counter(value: i64) -> i64 {\n    value + 1\n}\n";

describe("formatted source fallback", () => {
  test("shows the rustfmt rendering when present, with no note", () => {
    const shown = displayedSource(
      { lang: "rust", source: stored, formatted_source: formatted, format_error: null },
      false,
    );
    expect(shown).toEqual({ code: formatted, note: null, formatted: true });
  });
  test("falls back to the stored bytes and names the reason when formatting failed", () => {
    const shown = displayedSource(
      {
        lang: "rust",
        source: stored,
        formatted_source: null,
        format_error: "rustfmt exited with status 1: expected `;`",
      },
      false,
    );
    expect(shown.code).toBe(stored);
    expect(shown.formatted).toBe(false);
    expect(shown.note).toBe("Not formatted: rustfmt exited with status 1: expected `;`");
  });
  test("a null result without a reason is still visible", () => {
    const shown = displayedSource(
      { lang: "rust", source: stored, formatted_source: null, format_error: null },
      false,
    );
    expect(shown.note).toBe("Not formatted: the server gave no reason");
  });
  test("the as-submitted toggle shows the stored bytes even when a rendering exists", () => {
    const shown = displayedSource(
      { lang: "rust", source: stored, formatted_source: formatted, format_error: null },
      true,
    );
    expect(shown).toEqual({
      code: stored,
      note: "Exact stored bytes, as submitted.",
      formatted: false,
    });
  });
  test("non-Rust definitions show their source without a formatting note", () => {
    const shown = displayedSource(
      { lang: "typescript", source: "export const x = 1", formatted_source: null, format_error: null },
      false,
    );
    expect(shown).toEqual({ code: "export const x = 1", note: null, formatted: false });
  });
  test("splitLines drops only the final empty line", () => {
    expect(splitLines("a\nb\n")).toEqual(["a", "b"]);
    expect(splitLines("a\n\nb")).toEqual(["a", "", "b"]);
    expect(splitLines("")).toEqual([""]);
  });
});

import { expect, test } from "bun:test";
import { fragmentToken } from "../src/lib/workbench/fragment-token";

test("extracts and decodes the fragment token among other parameters", () => {
  expect(fragmentToken("#panel=view&token=abc%2Bdef%2Fghi%3D&x=1")).toBe(
    "abc+def/ghi=",
  );
  expect(fragmentToken("#token=a%26b%23c")).toBe("a&b#c");
});
test("does not accept a query string or unrelated fragment", () => {
  expect(fragmentToken("")).toBeNull();
  expect(fragmentToken("#section")).toBeNull();
  expect(fragmentToken("?token=secret")).toBeNull();
});
test("rejects empty and duplicate tokens without disclosing values", () => {
  for (const hash of ["#token=", "#token=%20", "#token=secret&token=other"])
    expect(() => fragmentToken(hash)).toThrow(
      "Fragment token: expected one nonempty token.",
    );
});

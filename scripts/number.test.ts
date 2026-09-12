import { expect, test } from "bun:test";
import { displayNumber } from "../ui/src/lib/number";
test("grouped integers and fractions retain JS precision", () => {
  const expected = new Intl.NumberFormat(undefined, {maximumSignificantDigits:21});
  for (const value of [918315, -918315, 1.2345678901234567, 0, -0]) expect(displayNumber(value)).toBe(expected.format(value));
  expect(displayNumber(918315)).not.toBe("918315");
});
test("tiny and large values remain nonzero scientific values", () => {
  const expected = new Intl.NumberFormat(undefined, {notation:"scientific",maximumSignificantDigits:21});
  expect(displayNumber(1e-30)).toBe(expected.format(1e-30));
  expect(displayNumber(-1e30)).toBe(expected.format(-1e30));
  expect(displayNumber(1e-30)).not.toBe(displayNumber(0));
});

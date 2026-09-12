import { expect, test } from "bun:test";
import { argumentHints } from "../src/lib/workbench/runArguments";
const one = {
  name: "one",
  params: [{ name: "count", shape: { type: "number" } }],
  returns: { type: "number" },
};
const two = {
  name: "two",
  params: [
    { name: "items", shape: { type: "array", items: { type: "number" } } },
  ],
  returns: { type: "null" },
};
test("selected entry owns its signature and nested array example", () => {
  const hints = argumentHints({
    entry: "two",
    def: { sig: { exports: [one, two] } },
  });
  expect(hints.signatures).toEqual(["two(items: Array<number>) → null"]);
  expect(JSON.parse(hints.example!)).toEqual([[]]);
});
test("single export can supply an example; multiple exports require a selected entry", () => {
  expect(
    JSON.parse(argumentHints({ def: { sig: { exports: [one] } } }).example!),
  ).toEqual([0]);
  expect(
    argumentHints({ entry: null, def: { sig: { exports: [one, two] } } })
      .example,
  ).toBeNull();
  expect(() =>
    argumentHints({ entry: "absent", def: { sig: { exports: [one] } } }),
  ).toThrow("absent");
});
test("object examples include required properties and omit optional properties", () => {
  const hints = argumentHints({
    def: {
      sig: {
        exports: [
          {
            ...one,
            params: [
              {
                name: "settings",
                shape: {
                  type: "object",
                  properties: {
                    enabled: { type: "boolean" },
                    label: { type: "string" },
                  },
                  optional: ["label"],
                },
              },
            ],
          },
        ],
      },
    },
  });
  expect(JSON.parse(hints.example!)).toEqual([{ enabled: false }]);
});

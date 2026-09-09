import { expect, test } from "bun:test";
import { signatures, shapeLabel } from "../loom-ui/src/lib/signature";
test("protocol signatures preserve names, arity, and shared TS/Rust shapes", () => {
  expect(signatures({exports:[{name:"main",params:[{name:"count",shape:{type:"number"}},{name:"files",shape:{type:"array",items:{type:"ref",target:{type:"string"}}}}],returns:{type:"boolean"}}]})).toEqual(["main(count: number, files: Array<Ref<string>>) → boolean"]);
  expect(signatures({exports:[{name:"clock",params:[],returns:{type:"number"}}]})).toEqual(["clock() → number"]);
});
test("optional fields remain visible and absent signature does not invent arguments", () => {
  expect(shapeLabel({type:"object",properties:{name:{type:"string"}},optional:["name"]})).toBe("{ name?: string }");
  expect(signatures(undefined)).toEqual([]);
  expect(shapeLabel({type:"future"})).toBe("unknown");
});
test("named definitions omit redundant main but retain other exports", () => {
  const sig = {exports:[{name:"main",params:[{name:"path",shape:{type:"string"}}],returns:{type:"value"}},{name:"helper",params:[],returns:{type:"null"}}]};
  expect(signatures(sig,"read-file")).toEqual(["(path: string) → Value","helper() → null"]);
});

import { expect, test } from "bun:test";
import { groupDefinitions, type JournalRow } from "../ui/src/lib/journal";
function definition(seq: number, hash: string, title = "worker"): JournalRow {
  return {id:`event-${seq}`,seq,kind:"defined",title,language:"ts",value:{hash},metadata:{seq}};
}
test("identical identities compact at latest occurrence while preserving provenance", () => {
  const original = [definition(1,"a"), {id:"eval",seq:2,kind:"evaluated",title:"call",language:"ts"}, definition(3,"a")];
  const grouped = groupDefinitions(original);
  expect(grouped.map(row => row.id)).toEqual(["eval","event-3"]);
  expect(grouped[1]?.occurrences?.map(row => row.seq)).toEqual([1,3]);
  expect(original[0]?.occurrences).toBeUndefined();
});
test("different identities or names remain separate", () => {
  expect(groupDefinitions([definition(1,"a"),definition(2,"b"),definition(3,"a","other")])).toHaveLength(3);
});

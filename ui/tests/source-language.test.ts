import { expect, test } from "bun:test";
import { get } from "svelte/store";
import { commandById, parseFields, V } from "../src/lib/workbench/commands";
import { Workspace } from "../src/lib/workbench/workspace";
import { WorkbenchClient } from "../src/lib/workbench/client";
import { definitionView } from "../src/lib/workbench/schema";
import { fixtures } from "../src/lib/workbench/mock";

test("new definitions submit TypeScript by default and permit explicit Rust", () => {
  const command = commandById(V.add);
  const session = new Workspace().open(command, {});
  const values = { ...get(session).values, source: "function main(value: number) { return value; }" };
  expect(parseFields(command, values).lang).toBe("typescript");
  expect(parseFields(command, { ...values, lang: "rust" }).lang).toBe("rust");
});

for (const lang of ["typescript", "javascript", "rust"]) {
  test(`editing preserves ${lang} grammar and source`, async () => {
    const original = fixtures.definitions[0]!;
    const view = { ...original, def: { ...original.def, lang } };
    if (lang !== "rust") {
      delete (view as { wasm_hash?: string }).wasm_hash;
      delete (view as { toolchain_hash?: string }).toolchain_hash;
    }
    const client = new WorkbenchClient({ request: async () => ({ ok: true, seq: 1, diagnostics: [], result: view }) });
    const session = new Workspace().open(commandById(V.update), { name: "counter" });
    await session.prepare(client);
    expect(get(session).error).toBe("");
    expect(get(session).values.lang).toBe(lang);
    expect(get(session).values.source).toBe(original.source);
    expect(definitionView(view).def.lang).toBe(lang);
  });
}

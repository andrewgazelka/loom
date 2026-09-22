import { FileCode2 } from "lucide-svelte";
import { buildOf, dependentsOf } from "../feed";
import DefinitionDetail from "./DefinitionDetail.svelte";
import { definePane, detailOf, type PaneIcon } from "./types";

export const pane = definePane({
  id: "definition-detail",
  title: "Definition",
  icon: FileCode2 as PaneIcon,
  area: "main",
  wide: true,
  shows: detailOf("def"),
  select: (model, selection) => {
    if (selection?.kind !== "def")
      throw new Error("definition detail: the selection is not a definition");
    const def = model.definitions[selection.hash] ?? null;
    return {
      hash: selection.hash,
      def,
      deps:
        def === null
          ? []
          : Object.entries(def.deps)
              .sort()
              .map(([alias, hash]) => ({
                alias,
                hash,
                name: model.definitions[hash]?.name ?? null,
              })),
      dependents: dependentsOf(model, selection.hash),
      names: Object.fromEntries(
        Object.values(model.definitions).map((item) => [item.hash, item.name]),
      ),
      build: def === null ? null : (buildOf(model, def) ?? null),
    };
  },
  component: DefinitionDetail,
});

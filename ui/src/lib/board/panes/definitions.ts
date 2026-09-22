import { FileCode2 } from "lucide-svelte";
import type { Definition } from "../feed";
import DefinitionsPane from "./DefinitionsPane.svelte";
import { definePane, type PaneIcon } from "./types";

function key(def: Definition): string {
  return `${def.name ?? "￿"}\n${def.hash}`;
}

export const pane = definePane({
  id: "definitions",
  title: "Definitions",
  icon: FileCode2 as PaneIcon,
  area: "rail",
  shows: () => true,
  select: (model, selection) => ({
    rows: Object.values(model.definitions).sort((a, b) =>
      key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0,
    ),
    active: model.activeUntil,
    selected: selection?.kind === "def" ? selection.hash : null,
  }),
  component: DefinitionsPane,
});

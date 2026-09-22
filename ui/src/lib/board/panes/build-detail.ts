import { Hammer } from "lucide-svelte";
import { definitionByComponent } from "../feed";
import BuildDetail from "./BuildDetail.svelte";
import { definePane, detailOf, type PaneIcon } from "./types";

export const pane = definePane({
  id: "build-detail",
  title: "Build",
  icon: Hammer as PaneIcon,
  area: "main",
  wide: true,
  shows: detailOf("build"),
  select: (model, selection) => {
    if (selection?.kind !== "build") throw new Error("build detail: the selection is not a build");
    return {
      hash: selection.hash,
      build: model.builds[selection.hash] ?? null,
      definition: definitionByComponent(model, selection.hash) ?? null,
    };
  },
  component: BuildDetail,
});

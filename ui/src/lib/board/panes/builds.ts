import { Hammer } from "lucide-svelte";
import { definitionByComponent } from "../feed";
import BuildsPane from "./BuildsPane.svelte";
import { definePane, overview, type PaneIcon } from "./types";

export const pane = definePane({
  id: "builds",
  title: "Builds",
  icon: Hammer as PaneIcon,
  area: "main",
  shows: overview,
  select: (model) => ({
    items: Object.values(model.builds)
      .sort((a, b) => b.seq - a.seq)
      .map((build) => {
        const definition = definitionByComponent(model, build.componentHash);
        return { build, name: definition?.name ?? null, definition: definition?.hash ?? null };
      }),
  }),
  component: BuildsPane,
});

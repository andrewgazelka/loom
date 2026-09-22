import { Share2 } from "lucide-svelte";
import { graphOf, layout } from "../layout";
import GraphPane from "./GraphPane.svelte";
import { definePane, overview, type PaneIcon } from "./types";

export const pane = definePane({
  id: "graph",
  title: "Graph",
  icon: Share2 as PaneIcon,
  area: "main",
  wide: true,
  shows: overview,
  select: (model, selection) => {
    const { nodes, edges } = graphOf(model);
    return {
      graph: layout(nodes, edges),
      active: model.activeUntil,
      selected: selection?.kind === "def" ? selection.hash : null,
    };
  },
  component: GraphPane,
});

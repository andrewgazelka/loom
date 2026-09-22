import { Network } from "lucide-svelte";
import ActorsPane from "./ActorsPane.svelte";
import { definePane, type PaneIcon } from "./types";

export const pane = definePane({
  id: "actors",
  title: "Actors",
  icon: Network as PaneIcon,
  area: "rail",
  shows: () => true,
  select: (model, selection) => ({
    rows: model.actorOrder.flatMap((id) => {
      const actor = model.actors[id];
      return actor ? [actor] : [];
    }),
    names: Object.fromEntries(
      Object.values(model.definitions).map((def) => [def.hash, def.name]),
    ),
    bumped: model.bumpedUntil,
    selected: selection?.kind === "actor" ? selection.id : null,
  }),
  component: ActorsPane,
});

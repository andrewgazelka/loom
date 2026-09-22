import { Network } from "lucide-svelte";
import ActorDetail from "./ActorDetail.svelte";
import { definePane, detailOf, type PaneIcon } from "./types";

export const pane = definePane({
  id: "actor-detail",
  title: "Actor",
  icon: Network as PaneIcon,
  area: "main",
  wide: true,
  shows: detailOf("actor"),
  select: (model, selection) => {
    if (selection?.kind !== "actor") throw new Error("actor detail: the selection is not an actor");
    const actor = model.actors[selection.id] ?? null;
    return {
      id: selection.id,
      actor,
      definitionName: actor === null ? null : (model.definitions[actor.hash]?.name ?? null),
    };
  },
  component: ActorDetail,
});

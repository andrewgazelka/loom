import { Activity } from "lucide-svelte";
import { runOf } from "../feed";
import RunDetail from "./RunDetail.svelte";
import { definePane, detailOf, type PaneIcon } from "./types";

export const pane = definePane({
  id: "run-detail",
  title: "Run",
  icon: Activity as PaneIcon,
  area: "main",
  wide: true,
  shows: detailOf("run"),
  select: (model, selection) => {
    if (selection?.kind !== "run") throw new Error("run detail: the selection is not a run");
    const run = runOf(model, selection.scope);
    const definitionName =
      run !== null && run.definitionHash !== null
        ? (model.definitions[run.definitionHash]?.name ?? null)
        : null;
    return { scope: selection.scope, run, definitionName };
  },
  component: RunDetail,
});

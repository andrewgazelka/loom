import { Activity } from "lucide-svelte";
import { summarize, targetOfRow } from "../feed";
import FeedPane from "./FeedPane.svelte";
import { definePane, overview, type PaneIcon } from "./types";

export const pane = definePane({
  id: "feed",
  title: "Feed",
  icon: Activity as PaneIcon,
  area: "main",
  shows: overview,
  select: (model) => ({
    items: model.feed.map((row) => ({
      row,
      summary: summarize(model, row),
      target: targetOfRow(model, row),
    })),
    held: model.feed.length,
  }),
  component: FeedPane,
});

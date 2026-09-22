/**
 * The pane registry. Adding a pane: write `panes/<Name>.svelte`, put its descriptor in
 * `panes/<name>.ts` beside it, and add one line here. The page composes from this list and the
 * saved arrangement (`./arrangement.ts`); nothing else names a pane.
 */
import { pane as definitions } from "./definitions";
import { pane as actors } from "./actors";
import { pane as graph } from "./graph";
import { pane as feed } from "./feed";
import { pane as builds } from "./builds";
import { pane as definitionDetail } from "./definition-detail";
import { pane as actorDetail } from "./actor-detail";
import { pane as buildDetail } from "./build-detail";
import { pane as runDetail } from "./run-detail";
import type { AnyPane } from "./types";

export const panes: AnyPane[] = [
  definitions,
  actors,
  graph,
  feed,
  builds,
  definitionDetail,
  actorDetail,
  buildDetail,
  runDetail,
];

const ids = new Set<string>();
for (const pane of panes) {
  if (ids.has(pane.id)) throw new Error(`pane registry: duplicate id ${pane.id}`);
  ids.add(pane.id);
}

export type { AnyPane, PaneDescriptor, PaneShared } from "./types";

/**
 * A board pane is one Svelte file plus one descriptor. The page composes panes from the
 * registry (`./index.ts`) and never names one: it calls `select` on the one model and spreads
 * the result into `component`, together with the shared callbacks below.
 */
import type { Component, SvelteComponent } from "svelte";
import type { IconProps } from "lucide-svelte";
import type { BoardClient } from "../connect";
import type { Model } from "../feed";
import type { Selection } from "../selection";

export type PaneArea = "rail" | "main";
export type PaneIcon = typeof SvelteComponent<IconProps>;

/** Props every pane receives in addition to what its `select` returned. */
export interface PaneShared {
  /** Open a Detail, or `null` for the Overview. */
  onselect: (selection: Selection | null) => void;
  /** The daemon connection; `null` until the page has one. Panes that fetch say so when null. */
  client: BoardClient | null;
}

export interface PaneDescriptor<Props extends Record<string, unknown>> {
  id: string;
  title: string;
  icon: PaneIcon;
  area: PaneArea;
  /** On screen for this selection: Overview panes answer `selection === null`, a Detail pane its kind. */
  shows: (selection: Selection | null) => boolean;
  /** Take a full-width cell of the main grid. */
  wide?: boolean;
  /** The only path from the model to the pane: nothing else reaches it. */
  select: (model: Model, selection: Selection | null) => Props;
  component: Component<Props & PaneShared>;
}
// The registry erases each pane's prop type; `definePane` keeps the check at the definition site.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type AnyPane = PaneDescriptor<any>;
export function definePane<Props extends Record<string, unknown>>(
  pane: PaneDescriptor<Props>,
): AnyPane {
  return pane;
}

export const overview = (selection: Selection | null) => selection === null;
export const detailOf =
  (kind: Selection["kind"]) => (selection: Selection | null) => selection?.kind === kind;

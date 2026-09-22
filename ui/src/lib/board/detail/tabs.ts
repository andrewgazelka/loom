/**
 * Detail tabs: one Svelte file plus one entry here. A Detail pane builds a `DetailContext` from
 * what it loaded, `DetailTabs.svelte` filters this registry with `applies(selection)`, draws the
 * segmented control from the ids (`data-tab=<id>`), and gives the active tab only what its
 * `select` returns.
 */
import type { Component } from "svelte";
import type { BoardClient } from "../connect";
import type { Actor, Build, Definition } from "../feed";
import type { Selection } from "../selection";
import type { DefinitionView } from "../../workbench/schema";
import SourceTab from "./SourceTab.svelte";
import WasmTab from "./WasmTab.svelte";
import ItemsTab from "./ItemsTab.svelte";
import HistoryTab from "./HistoryTab.svelte";
import TablesTab from "./TablesTab.svelte";
import LineageTab from "./LineageTab.svelte";
import StagesTab from "./StagesTab.svelte";
import LogTab from "./LogTab.svelte";

/** What a Detail pane hands to its tabs; each tab's `select` picks its part. */
export type DetailContext =
  | {
      kind: "def";
      def: Definition;
      /** The `view` verb result, `null` until loaded (the tab says "loading" or shows `viewError`). */
      view: DefinitionView | null;
      viewError: string | null;
      client: BoardClient | null;
      onselect: (selection: Selection | null) => void;
      names: Record<string, string | null>;
    }
  | { kind: "actor"; actor: Actor; client: BoardClient | null }
  | { kind: "build"; build: Build; client: BoardClient | null };

export interface TabDescriptor<Props extends Record<string, unknown>> {
  id: string;
  title: string;
  applies: (selection: Selection) => boolean;
  /** Narrow the context to the tab's props; called only when `applies` held for the selection. */
  select: (context: DetailContext) => Props;
  component: Component<Props>;
}
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type AnyTab = TabDescriptor<any>;
function defineTab<Props extends Record<string, unknown>>(tab: TabDescriptor<Props>): AnyTab {
  return tab;
}

const forDef = (selection: Selection) => selection.kind === "def";
const forActor = (selection: Selection) => selection.kind === "actor";
const forBuild = (selection: Selection) => selection.kind === "build";
function narrow<K extends DetailContext["kind"]>(
  context: DetailContext,
  kind: K,
): Extract<DetailContext, { kind: K }> {
  if (context.kind !== kind) throw new Error(`detail tab: expected a ${kind} context, got ${context.kind}`);
  return context as Extract<DetailContext, { kind: K }>;
}

export const tabs: AnyTab[] = [
  defineTab({
    id: "source",
    title: "Source",
    applies: forDef,
    select: (context) => {
      const { def, view, viewError } = narrow(context, "def");
      return { lang: def.lang, view, viewError };
    },
    component: SourceTab,
  }),
  defineTab({
    id: "wasm",
    title: "Wasm",
    applies: forDef,
    select: (context) => {
      const { def, view, viewError, client } = narrow(context, "def");
      return {
        lang: def.lang,
        componentHash: def.componentHash,
        exports: def.exports,
        view,
        viewError,
        client,
      };
    },
    component: WasmTab,
  }),
  defineTab({
    id: "items",
    title: "Items",
    applies: forDef,
    select: (context) => {
      const { view, viewError } = narrow(context, "def");
      return { items: view?.items ?? null, viewError };
    },
    component: ItemsTab,
  }),
  defineTab({
    id: "history",
    title: "History",
    applies: forDef,
    select: (context) => {
      const { def, client, onselect, names } = narrow(context, "def");
      return { name: def.name, hash: def.hash, client, onselect, names };
    },
    component: HistoryTab,
  }),
  defineTab({
    id: "tables",
    title: "Tables",
    applies: forActor,
    select: (context) => {
      const { actor, client } = narrow(context, "actor");
      return { id: actor.id, client };
    },
    component: TablesTab,
  }),
  defineTab({
    id: "lineage",
    title: "Lineage",
    applies: forActor,
    select: (context) => {
      const { actor, client } = narrow(context, "actor");
      return { id: actor.id, client };
    },
    component: LineageTab,
  }),
  defineTab({
    id: "stages",
    title: "Stages",
    applies: forBuild,
    select: (context) => {
      const { build } = narrow(context, "build");
      return { build };
    },
    component: StagesTab,
  }),
  defineTab({
    id: "log",
    title: "Log",
    applies: forBuild,
    select: (context) => {
      const { build, client } = narrow(context, "build");
      return { logsRef: build.logsRef, componentHash: build.componentHash, client };
    },
    component: LogTab,
  }),
];

export function tabsFor(selection: Selection): AnyTab[] {
  return tabs.filter((tab) => tab.applies(selection));
}

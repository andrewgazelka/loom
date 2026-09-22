/**
 * Which panes are on screen and where. The arrangement is a list of pane ids per area plus the
 * hidden set, saved in localStorage under `loom.board.layout`; the registry is the truth about
 * which ids exist, the saved arrangement only orders and hides them.
 */
import type { Selection } from "../selection";
import type { AnyPane, PaneArea } from "./types";

export const LAYOUT_KEY = "loom.board.layout";

export interface Arrangement {
  version: 1;
  areas: Record<PaneArea, string[]>;
  hidden: string[];
}

export function defaultArrangement(registry: AnyPane[]): Arrangement {
  return {
    version: 1,
    areas: {
      rail: registry.filter((pane) => pane.area === "rail").map((pane) => pane.id),
      main: registry.filter((pane) => pane.area === "main").map((pane) => pane.id),
    },
    hidden: [],
  };
}

function strings(value: unknown, what: string): string[] {
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string"))
    throw new Error(`${LAYOUT_KEY}: ${what} must be a list of pane ids`);
  return value as string[];
}

/**
 * Read the saved arrangement and reconcile it with the registry: ids the registry no longer
 * has are dropped, panes the registry gained are appended to their area. Malformed JSON or a
 * wrong version fails by name; the caller shows the error and falls back to the default.
 */
export function loadArrangement(
  storage: Pick<Storage, "getItem">,
  registry: AnyPane[],
): Arrangement {
  const fallback = defaultArrangement(registry);
  const raw = storage.getItem(LAYOUT_KEY);
  if (raw === null) return fallback;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new Error(`${LAYOUT_KEY}: saved layout is not JSON`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed))
    throw new Error(`${LAYOUT_KEY}: saved layout must be an object`);
  const saved = parsed as Record<string, unknown>;
  if (saved.version !== 1) throw new Error(`${LAYOUT_KEY}: unknown layout version`);
  const areas =
    typeof saved.areas === "object" && saved.areas !== null
      ? (saved.areas as Record<string, unknown>)
      : {};
  const known = new Map(registry.map((pane) => [pane.id, pane]));
  const reconcile = (area: PaneArea): string[] => {
    const ordered = strings(areas[area] ?? [], `areas.${area}`).filter(
      (id) => known.get(id)?.area === area,
    );
    for (const id of fallback.areas[area]) if (!ordered.includes(id)) ordered.push(id);
    return ordered;
  };
  return {
    version: 1,
    areas: { rail: reconcile("rail"), main: reconcile("main") },
    hidden: strings(saved.hidden ?? [], "hidden").filter((id) => known.has(id)),
  };
}

export function saveArrangement(storage: Pick<Storage, "setItem">, arrangement: Arrangement) {
  storage.setItem(LAYOUT_KEY, JSON.stringify(arrangement));
}

export function toggleHidden(arrangement: Arrangement, id: string): Arrangement {
  const hidden = arrangement.hidden.includes(id)
    ? arrangement.hidden.filter((item) => item !== id)
    : [...arrangement.hidden, id];
  return { ...arrangement, hidden };
}

export interface Cell {
  pane: AnyPane;
  /** Spans the whole main grid row. */
  full: boolean;
}
export interface Composed {
  rail: AnyPane[];
  main: Cell[];
}

/**
 * The panes on screen for a selection, in area order: visible, not hidden, and `shows(selection)`.
 * Wide panes take a full row; the narrow ones pair up two per row, and an odd last one widens so
 * hiding a pane leaves no empty cell.
 */
export function compose(
  registry: AnyPane[],
  arrangement: Arrangement,
  selection: Selection | null,
): Composed {
  const byId = new Map(registry.map((pane) => [pane.id, pane]));
  const visible = (area: PaneArea): AnyPane[] =>
    arrangement.areas[area].flatMap((id) => {
      const pane = byId.get(id);
      return pane && !arrangement.hidden.includes(id) && pane.shows(selection) ? [pane] : [];
    });
  const main = visible("main");
  const narrow = main.filter((pane) => !pane.wide);
  const lastNarrow = narrow.length % 2 === 1 ? narrow[narrow.length - 1] : undefined;
  return {
    rail: visible("rail"),
    main: main.map((pane) => ({ pane, full: pane.wide === true || pane === lastNarrow })),
  };
}

/** Panes the "panes" menu offers: those that appear in the Overview. Detail panes follow the selection instead. */
export function toggleablePanes(registry: AnyPane[]): AnyPane[] {
  return registry.filter((pane) => pane.shows(null));
}

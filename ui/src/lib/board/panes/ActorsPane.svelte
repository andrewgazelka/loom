<script lang="ts">
  import { Circle, Network } from "lucide-svelte";
  import type { Actor } from "../feed";
  import type { PaneShared } from "./types";
  let {
    rows,
    names,
    bumped,
    selected,
    onselect,
  }: {
    /** Supervision tree preorder. */
    rows: Actor[];
    /** Definition hash to name, for the actors' behaviors. */
    names: Record<string, string | null>;
    /** Actor ids whose cursor just moved. */
    bumped: Record<string, number>;
    selected: string | null;
  } & PaneShared = $props();
  function activate(event: KeyboardEvent, id: string) {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      onselect({ kind: "actor", id });
    }
  }
</script>

<div class="section-bar">
  <Network size={14} class="icon-actor" />
  <h2>Actors</h2>
  <span class="muted">{rows.length}</span>
</div>
<div class="list" data-pane="actors" role="listbox" aria-label="Supervision tree, polled every second">
  {#each rows as actor (actor.id)}
    <div
      class="row"
      role="option"
      tabindex="0"
      data-row
      data-testid="board-actor"
      data-id={actor.id}
      data-cursor={actor.cursor}
      aria-selected={selected === actor.id}
      class:bumped={bumped[actor.id] !== undefined}
      style={`--depth:${actor.depth}`}
      onclick={() => onselect({ kind: "actor", id: actor.id })}
      onkeydown={(event) => activate(event, actor.id)}
    >
      <Circle size={7} class={`status-icon ${actor.status}`} />
      <span class="id" title={`${actor.id} · ${actor.status}`}>{actor.id}</span>
      <span class="def muted">{names[actor.hash] ?? (actor.hash ? actor.hash.slice(0, 8) : "")}</span>
      <span class="numeric cursor" title="cursor">{actor.cursor}</span>
    </div>
  {:else}
    <div class="empty">No actors reported by the tree poll.</div>
  {/each}
</div>

<style>
  .list {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 28px;
    padding: 2px 10px 2px calc(10px + var(--depth) * 14px);
    border-bottom: 1px solid var(--line);
    outline: none;
    cursor: pointer;
  }
  .row:hover {
    background: var(--code);
  }
  .row[aria-selected="true"] {
    background: var(--selection);
  }
  .row:focus-visible {
    outline: 1px solid var(--focus);
    outline-offset: -1px;
  }
  .row.bumped {
    box-shadow: inset 2px 0 0 var(--ink);
  }
  .id {
    font-family: var(--mono);
    font-size: 0.92em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .def {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 0.9em;
  }
  .cursor {
    min-width: 3ch;
    text-align: right;
    color: var(--muted);
  }
</style>

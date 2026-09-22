<script lang="ts">
  import { FileCode2 } from "lucide-svelte";
  import type { Definition } from "../feed";
  import type { PaneShared } from "./types";
  let {
    rows,
    active,
    selected,
    onselect,
  }: {
    rows: Definition[];
    /** Definition hashes lit by a recent call. */
    active: Record<string, number>;
    selected: string | null;
  } & PaneShared = $props();
  function activate(event: KeyboardEvent, hash: string) {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      onselect({ kind: "def", hash });
    }
  }
</script>

<div class="section-bar">
  <FileCode2 size={14} class="icon-code" />
  <h2>Definitions</h2>
  <span class="muted">{rows.length}</span>
</div>
<div class="list" data-pane="definitions" role="listbox" aria-label="Definitions">
  {#each rows as def (def.hash)}
    <div
      class="row"
      role="option"
      tabindex="0"
      data-row
      data-testid="board-def"
      data-hash={def.hash}
      data-name={def.name ?? ""}
      aria-selected={selected === def.hash}
      class:active={active[def.hash] !== undefined}
      onclick={() => onselect({ kind: "def", hash: def.hash })}
      onkeydown={(event) => activate(event, def.hash)}
    >
      <span class="name">{def.name ?? def.hash.slice(0, 8)}</span>
      <span class="meta muted">{def.lang}</span>
      {#each def.effects as label (label)}<span class="effect" class:call={label === "call"}
          >{label}</span
        >{/each}
    </div>
  {:else}
    <div class="empty">No definitions yet. Add one and it appears here.</div>
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
    padding: 3px 10px;
    border-bottom: 1px solid var(--line);
    cursor: pointer;
    outline: none;
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
  .row.active {
    box-shadow: inset 2px 0 0 var(--ink);
  }
  .name {
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
    min-width: 0;
  }
  .meta {
    font-size: 0.86em;
    white-space: nowrap;
  }
  .effect {
    font: 0.78em var(--mono);
    border: 1px solid var(--line);
    border-radius: 3px;
    padding: 0 4px;
    color: var(--muted);
  }
  .effect.call {
    color: var(--verdict-red);
    border-color: var(--verdict-red);
  }
</style>

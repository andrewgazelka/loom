<script lang="ts">
  import { Circle, Network } from "lucide-svelte";
  import Hash from "../workbench/Hash.svelte";
  import type { Model } from "./feed";
  let { model }: { model: Model } = $props();
  const rows = $derived(
    model.actorOrder.flatMap((id) => {
      const actor = model.actors[id];
      return actor ? [actor] : [];
    }),
  );
</script>

<div class="section-bar">
  <Network size={14} class="icon-actor" />
  <h2>Actors</h2>
  <span class="muted">{rows.length}</span>
  <span class="push muted heading">status · cursor</span>
</div>
<div class="list" data-pane="actors" role="list" aria-label="Supervision tree, polled every second">
  {#each rows as actor (actor.id)}
    {@const name = model.definitions[actor.hash]?.name ?? null}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="row"
      role="listitem"
      tabindex="0"
      data-row
      data-testid="board-actor"
      data-id={actor.id}
      data-cursor={actor.cursor}
      class:bumped={model.bumpedUntil[actor.id] !== undefined}
      style={`--depth:${actor.depth}`}
    >
      <Circle size={7} class={`status-icon ${actor.status}`} />
      <span class="id" title={actor.id}>{actor.id}</span>
      {#if name !== null}<span class="def muted">{name}</span
        >{:else if actor.hash}<Hash value={actor.hash} />{/if}
      <span class="status muted">{actor.status}</span>
      <span class="numeric cursor">{actor.cursor}</span>
    </div>
  {:else}
    <div class="empty">No actors reported by the tree poll.</div>
  {/each}
</div>

<style>
  .heading {
    font-size: 0.8em;
  }
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
  }
  .row:hover {
    background: var(--code);
  }
  .row:focus-visible {
    background: var(--selection);
  }
  .row.bumped {
    background: var(--selection);
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
  }
  .status {
    margin-left: auto;
    font-size: 0.86em;
  }
  .cursor {
    min-width: 3ch;
    text-align: right;
  }
</style>

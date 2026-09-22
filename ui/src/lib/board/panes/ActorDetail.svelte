<script lang="ts">
  /** One actor in focus: status, cursor, its definition and parent on one line, then Tables and Lineage. */
  import { Circle } from "lucide-svelte";
  import type { Actor } from "../feed";
  import DetailTabs from "../detail/DetailTabs.svelte";
  import { short } from "../detail/format";
  import type { PaneShared } from "./types";
  let {
    id,
    actor,
    definitionName,
    onselect,
    client,
  }: {
    id: string;
    /** `null` when the tree poll has not reported the id. */
    actor: Actor | null;
    definitionName: string | null;
  } & PaneShared = $props();
</script>

{#if actor === null}
  <div class="note">Actor {id} is not in the supervision tree this board polls.</div>
{:else}
  <header class="head" data-testid="detail-actor" data-id={actor.id}>
    <Circle size={9} class={`status-icon ${actor.status}`} />
    <h1 class="name">{actor.id}</h1>
    <span class="pill">{actor.status}</span>
    <span class="muted">cursor</span><span class="numeric">{actor.cursor}</span>
    {#if actor.hash}
      <span class="muted">definition</span>
      <button
        type="button"
        class="text-control def"
        data-hash={actor.hash}
        title={actor.hash}
        onclick={() => onselect({ kind: "def", hash: actor.hash })}
        >{definitionName ?? short(actor.hash)}</button
      >
    {/if}
    {#if actor.parent !== null}
      <span class="muted">under</span>
      <button
        type="button"
        class="text-control def"
        onclick={() => onselect({ kind: "actor", id: actor.parent! })}>{actor.parent}</button
      >
    {/if}
  </header>
  <DetailTabs selection={{ kind: "actor", id: actor.id }} context={{ kind: "actor", actor, client }} />
{/if}

<style>
  .head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 10px;
    min-height: 40px;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    flex: none;
  }
  .name {
    font-family: var(--mono);
    font-size: 1.1em;
    font-weight: 600;
  }
  .pill {
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 0 5px;
    font-size: 0.86em;
    color: var(--muted);
    background: var(--side);
  }
  .def {
    font-weight: 500;
  }
</style>

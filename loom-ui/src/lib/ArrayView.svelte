<script lang="ts">
  import ValueView from "./ValueView.svelte";
  export let value: unknown[];
  export let depth = 0;
  export let inspect: (hash: string) => void;
  let limit = 10;
</script>

<ol class="array-values">
  {#each value.slice(0, limit) as item, index}<li>
      <span class="index">{index}</span><ValueView
        value={item}
        depth={depth + 1}
        {inspect}
      />
    </li>{:else}<li class="empty-array">[]</li>{/each}
</ol>
{#if value.length > limit}<button class="more" on:click={() => (limit += 50)}
    >Show {Math.min(50, value.length - limit)} more items</button
  >{/if}

<style>
  .array-values {
    padding: 0 0 0 12px;
    margin: 2px 0 0 3px;
    border-left: 1px solid var(--line);
    list-style: none;
  }
  .array-values li {
    display: flex;
    gap: 12px;
    align-items: baseline;
    padding: 3px 0;
  }
  .index {
    color: var(--muted);
    font: 9px var(--mono);
    min-width: 15px;
  }
  .empty-array,
  .more {
    color: var(--muted);
    font: 10px var(--mono);
  }
  .more {
    padding: 8px 15px;
  }
</style>

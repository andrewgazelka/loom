<script lang="ts">
  /** The definition's items: one row per item name with its content hash. */
  import { short } from "./format";
  let {
    items,
    viewError,
  }: { items: Record<string, string> | null; viewError: string | null } = $props();
  const rows = $derived(items === null ? [] : Object.entries(items).sort());
</script>

{#if viewError !== null}
  <div class="error" role="alert">{viewError}</div>
{:else if items === null}
  <div class="note">Loading items…</div>
{:else}
  <div class="table-wrap">
    <table aria-label="Definition items">
      <thead><tr><th>Item</th><th>Hash</th></tr></thead>
      <tbody>
        {#each rows as [name, hash] (name)}
          <tr><td class="name">{name}</td><td class="hash" title={hash}>{short(hash)}</td></tr>
        {:else}
          <tr><td colspan="2" class="empty">No items.</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}

<style>
  .name {
    font-family: var(--mono);
  }
  .hash {
    font-family: var(--mono);
    color: var(--muted);
  }
</style>

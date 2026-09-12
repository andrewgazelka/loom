<script lang="ts">
  import ValueView from "./ValueView.svelte";
  export let value: Record<string, unknown>;
  export let depth = 0;
  export let inspect: (hash: string) => void;
</script>

<div class="object-fields">
  {#each Object.keys(value) as name}<ValueView
      value={value[name]}
      label={name}
      depth={depth + 1}
      {inspect}
    />{:else}<span class="empty-object">{"{}"}</span>{/each}
</div>

<style>
  .object-fields {
    padding-left: 17px;
    border-left: 1px solid var(--line);
    margin-left: 3px;
  }
  .empty-object {
    color: var(--muted);
    font: 11px var(--mono);
  }
</style>

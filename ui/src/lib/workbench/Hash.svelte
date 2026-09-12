<script lang="ts">
  import { Copy, Check } from "lucide-svelte";
  export let value: string | null;
  export let full = false;
  let copied = false;
  let error = "";
  async function copy() {
    if (value === null) return;
    try {
      await navigator.clipboard.writeText(value);
      copied = true;
      error = "";
    } catch (problem) {
      error = `Copy hash: ${String(problem)}`;
    }
  }
  $: if (value) copied = false;
</script>

<span class="hash" class:full title={value ?? "No hash"}>
  <code>{value === null ? "—" : value.slice(0, 8)}</code>
  {#if value !== null}<button
      type="button"
      aria-label={`Copy hash ${value}`}
      on:click|stopPropagation={copy}
      >{#if copied}<Check size={10} />{:else}<Copy size={10} />{/if}</button
    >{/if}
</span>
{#if error}<span role="alert">{error}</span>{/if}

<style>
  .hash {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    border: 1px solid var(--line);
    background: var(--side);
    border-radius: 4px;
    padding: 1px 4px;
    vertical-align: middle;
    white-space: nowrap;
  }
  code {
    font-size: 0.92em;
    color: var(--muted);
  }
  button {
    display: inline-flex;
    padding: 2px;
    color: var(--muted);
  }
  .full {
    user-select: all;
  }
</style>

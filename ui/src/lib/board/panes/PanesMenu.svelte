<script lang="ts">
  /** The header's "panes" menu: one checkbox row per Overview pane; our own popover, no native control. */
  import { LayoutGrid, Check } from "lucide-svelte";
  import type { AnyPane } from "./types";
  let {
    options,
    hidden,
    ontoggle,
  }: { options: AnyPane[]; hidden: string[]; ontoggle: (id: string) => void } = $props();
  let open = $state(false);
  let root = $state<HTMLElement | null>(null);
  function outside(event: PointerEvent) {
    if (open && root && !root.contains(event.target as Node)) open = false;
  }
  function keyboard(event: KeyboardEvent) {
    if (event.key === "Escape" && open) {
      event.stopPropagation();
      open = false;
    }
  }
</script>

<svelte:window onpointerdown={outside} />
<div class="panes-menu" bind:this={root} onkeydown={keyboard} role="presentation">
  <button
    type="button"
    class="trigger"
    aria-haspopup="true"
    aria-expanded={open}
    aria-label="Panes"
    title="Show or hide panes"
    data-testid="panes-menu"
    onclick={() => (open = !open)}><LayoutGrid size={14} /><span>panes</span
    >{#if hidden.length}<span class="count">{hidden.length} hidden</span>{/if}</button
  >
  {#if open}
    <div class="menu" role="group" aria-label="Overview panes">
      {#each options as pane (pane.id)}
        {@const shown = !hidden.includes(pane.id)}
        <button
          type="button"
          role="checkbox"
          aria-checked={shown}
          data-pane-toggle={pane.id}
          onclick={() => ontoggle(pane.id)}
        >
          <span class="box">{#if shown}<Check size={11} />{/if}</span>
          <pane.icon size={13} />
          <span>{pane.title}</span>
          <span class="muted area">{pane.area}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .panes-menu {
    position: relative;
  }
  .trigger {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 1px 6px;
    font-size: 0.9em;
  }
  .trigger:hover,
  .trigger[aria-expanded="true"] {
    color: var(--ink);
    background: var(--selection);
  }
  .count {
    font-family: var(--mono);
    font-size: 0.9em;
  }
  .menu {
    position: absolute;
    right: 0;
    top: calc(100% + 6px);
    z-index: 20;
    min-width: 200px;
    padding: 4px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--card);
    display: grid;
  }
  .menu button {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    padding: 5px 8px;
    border-radius: 4px;
    color: var(--ink);
  }
  .menu button:hover {
    background: var(--selection);
  }
  .box {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 14px;
    height: 14px;
    border: 1px solid var(--line);
    border-radius: 3px;
    background: var(--bg);
  }
  .area {
    margin-left: auto;
    font-size: 0.85em;
  }
</style>

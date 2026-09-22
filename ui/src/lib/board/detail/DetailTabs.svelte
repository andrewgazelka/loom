<script lang="ts">
  /**
   * The segmented control and body of a Detail: tabs come from the registry, filtered by the
   * selection; the active tab receives only what its `select` picks from the context. The parent
   * keys this component by selection, so the first applicable tab is the default for a new one.
   */
  import Segmented from "../Segmented.svelte";
  import type { Selection } from "../selection";
  import { memo, sameRecord } from "../stable.svelte";
  import { tabsFor, type DetailContext } from "./tabs";
  let { selection, context }: { selection: Selection; context: DetailContext } =
    $props();
  const available = $derived(tabsFor(selection));
  let chosen = $state<string | null>(null);
  const active = $derived(
    available.find((tab) => tab.id === chosen) ?? available[0] ?? null,
  );
  // The parent rebuilds `context` on every board event; the tab sees a new props object only
  // when one of the values its `select` picked actually changed.
  const tabProps = memo(
    (): Record<string, unknown> =>
      active === null ? {} : active.select(context),
    sameRecord,
  );
</script>

{#if available.length}
  <div class="tab-bar">
    <Segmented
      label="Detail sections"
      options={available.map((tab) => ({ value: tab.id, label: tab.title }))}
      value={active?.id ?? ""}
      onchange={(value) => (chosen = value)}
    />
  </div>
  {#if active}
    {@const Tab = active.component}
    <div class="tab-body" data-tab-body={active.id}>
      <Tab {...tabProps.value} />
    </div>
  {/if}
{/if}

<style>
  .tab-bar {
    display: flex;
    align-items: center;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    background: var(--side);
    flex: none;
  }
  .tab-body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: auto;
  }
</style>

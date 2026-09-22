<script lang="ts">
  /** One component build in focus: what was built and how long, then Stages (large bars) and Log. */
  import type { Build, Definition } from "../feed";
  import DetailTabs from "../detail/DetailTabs.svelte";
  import { bytes, short, stamp } from "../detail/format";
  import type { PaneShared } from "./types";
  let {
    hash,
    build,
    definition,
    onselect,
    client,
  }: {
    hash: string;
    /** `null` when the journal has no `component_built` event for the hash. */
    build: Build | null;
    definition: Definition | null;
  } & PaneShared = $props();
</script>

{#if build === null}
  <div class="note">Build {short(hash)} is not in the journal this board has read.</div>
{:else}
  <header class="head" data-testid="detail-build" data-hash={build.componentHash}>
    <h1 class="name">
      {#if definition}<button
          type="button"
          class="text-control def"
          data-hash={definition.hash}
          onclick={() => onselect({ kind: "def", hash: definition.hash })}
          >{definition.name ?? short(definition.hash)}</button
        >{:else}unnamed component{/if}
    </h1>
    <span class="pill mono" title={build.componentHash}>{short(build.componentHash)}</span>
    {#if build.ms !== null}<span class="numeric">{build.ms} ms</span>{/if}
    {#if build.size !== null}<span class="muted numeric">{bytes(build.size)}</span>{/if}
    {#if build.rustcInvocations !== null}<span class="muted">{build.rustcInvocations} rustc</span
      >{/if}
    <span class="push muted numeric">{stamp(build.ts)} · seq {build.seq}</span>
  </header>
  <DetailTabs
    selection={{ kind: "build", hash: build.componentHash }}
    context={{ kind: "build", build, client }}
  />
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
    font-size: 1.15em;
    font-weight: 600;
  }
  .def {
    font: inherit;
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
  .mono {
    font-family: var(--mono);
  }
</style>

<script lang="ts">
  import { Hammer } from "lucide-svelte";
  import Hash from "../workbench/Hash.svelte";
  import { definitionByComponent, type Model } from "./feed";
  import { sortedStages } from "./stages";
  let { model }: { model: Model } = $props();
  const rows = $derived(
    Object.values(model.builds).sort((a, b) => b.seq - a.seq),
  );
</script>

<div class="section-bar">
  <Hammer size={14} class="icon-code" />
  <h2>Builds</h2>
  <span class="muted">{rows.length}</span>
</div>
<div class="list" data-pane="builds" role="list" aria-label="Component builds, newest first">
  {#each rows as build (build.componentHash)}
    {@const def = definitionByComponent(model, build.componentHash)}
    {@const stages = build.stages === null ? [] : sortedStages(build.stages)}
    {@const max = stages.reduce((most, [, ms]) => Math.max(most, ms), 0)}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="build"
      role="listitem"
      tabindex="0"
      data-row
      data-testid="board-build"
      data-hash={build.componentHash}
      data-definition={def?.hash ?? ""}
    >
      <div class="head">
        <span class="name">{def?.name ?? "unnamed"}</span>
        <Hash value={build.componentHash} />
        {#if build.ms !== null}<span class="numeric">{build.ms} ms</span>{/if}
        {#if build.rustcInvocations !== null}<span class="muted"
            >{build.rustcInvocations} rustc</span
          >{/if}
        <span class="seq numeric muted">{build.seq}</span>
      </div>
      {#if build.stagesError !== null}
        <div class="failure">{build.stagesError}</div>
      {:else if build.stages === null}
        <div class="note muted">
          {build.logsRef === null ? "No build log recorded." : "Reading build log…"}
        </div>
      {:else if !stages.length}
        <div class="note muted">The log has no build_stages line.</div>
      {:else}
        <div class="bars">
          {#each stages as [name, ms] (name)}
            <div class="stage">
              <span class="stage-name" title={name}>{name}</span>
              <div class="track">
                <div
                  class="bar"
                  class:other={name === "unattributed_ms"}
                  data-stage={name}
                  style={`width:${max ? (ms / max) * 100 : 0}%`}
                ></div>
              </div>
              <span class="numeric value">{ms} ms</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {:else}
    <div class="empty">No component builds.</div>
  {/each}
</div>

<style>
  .list {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .build {
    padding: 6px 10px;
    border-bottom: 1px solid var(--line);
    outline: none;
  }
  .build:focus-visible {
    background: var(--selection);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    white-space: nowrap;
  }
  .name {
    font-weight: 500;
  }
  .seq {
    margin-left: auto;
    font-size: 0.86em;
  }
  .note,
  .failure {
    padding: 4px 0 0;
    font-size: 0.9em;
  }
  .failure {
    color: var(--error);
    overflow-wrap: anywhere;
  }
  .bars {
    display: grid;
    gap: 3px;
    padding-top: 5px;
  }
  .stage {
    display: grid;
    grid-template-columns: minmax(70px, 110px) minmax(0, 1fr) 7ch;
    align-items: center;
    gap: 8px;
    font-size: 0.9em;
  }
  .stage-name {
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: var(--mono);
    font-size: 0.92em;
  }
  .track {
    height: 9px;
    background: var(--side);
    border-radius: 2px;
    overflow: hidden;
  }
  .bar {
    height: 100%;
    min-width: 1px;
    background: var(--series-a);
  }
  .bar.other {
    background: var(--series-b);
  }
  .value {
    text-align: right;
  }
</style>

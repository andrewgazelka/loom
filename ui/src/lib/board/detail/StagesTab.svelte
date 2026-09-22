<script lang="ts">
  /** The build's stage breakdown, large: the same bars as the Builds pane, one per row. */
  import type { Build } from "../feed";
  import { sortedStages } from "../stages";
  let { build }: { build: Build } = $props();
  const stages = $derived(build.stages === null ? [] : sortedStages(build.stages));
  const max = $derived(stages.reduce((most, [, ms]) => Math.max(most, ms), 0));
  const total = $derived(stages.reduce((sum, [, ms]) => sum + ms, 0));
</script>

{#if build.stagesError !== null}
  <div class="error" role="alert">{build.stagesError}</div>
{:else if build.stages === null}
  <div class="note">{build.logsRef === null ? "No build log recorded." : "Reading build log…"}</div>
{:else if !stages.length}
  <div class="note">The log has no build_stages line.</div>
{:else}
  <div class="stages" data-testid="detail-stages">
    {#each stages as [stage, ms] (stage)}
      <div class="stage">
        <span class="stage-name" title={stage}>{stage}</span>
        <div class="track">
          <div
            class="bar"
            class:other={stage === "unattributed_ms"}
            data-stage={stage}
            style={`width:${max ? (ms / max) * 100 : 0}%`}
          ></div>
        </div>
        <span class="numeric value">{ms} ms</span>
        <span class="numeric muted share">{total ? Math.round((ms / total) * 100) : 0}%</span>
      </div>
    {/each}
  </div>
{/if}

<style>
  .stages {
    display: grid;
    gap: 10px;
    padding: 16px;
  }
  .stage {
    display: grid;
    grid-template-columns: minmax(120px, 200px) minmax(0, 1fr) 9ch 5ch;
    align-items: center;
    gap: 12px;
  }
  .stage-name {
    font-family: var(--mono);
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .track {
    height: 18px;
    background: var(--side);
    border-radius: 3px;
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
  .value,
  .share {
    text-align: right;
  }
</style>

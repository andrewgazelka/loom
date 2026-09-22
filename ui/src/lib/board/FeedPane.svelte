<script lang="ts">
  import { Activity } from "lucide-svelte";
  import Hash from "../workbench/Hash.svelte";
  import { summarize, type FeedRow, type Model, type RowSummary } from "./feed";
  let { model }: { model: Model } = $props();
  let hidden = $state<string[]>([]);
  let query = $state("");
  const types = $derived(
    [...new Set(model.feed.map((row) => row.type))].sort(),
  );
  function matches(row: FeedRow, summary: RowSummary): boolean {
    const needle = query.trim().toLowerCase();
    if (!needle) return true;
    return (
      row.type.includes(needle) ||
      (summary.name ?? "").toLowerCase().includes(needle) ||
      (summary.hash ?? "").startsWith(needle) ||
      (summary.actor ?? "").toLowerCase().includes(needle) ||
      String(row.seq) === needle
    );
  }
  const rows = $derived(
    model.feed
      .map((row) => ({ row, summary: summarize(model, row) }))
      .filter(
        ({ row, summary }) => !hidden.includes(row.type) && matches(row, summary),
      ),
  );
  function toggle(type: string) {
    hidden = hidden.includes(type)
      ? hidden.filter((item) => item !== type)
      : [...hidden, type];
  }
  /** HH:MM:SS in the browser's zone; `ts` is Unix seconds. */
  function time(ts: number): string {
    const date = new Date(ts * 1000);
    const pad = (value: number) => String(value).padStart(2, "0");
    return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  }
</script>

<div class="section-bar">
  <Activity size={14} class="icon-run" />
  <h2>Feed</h2>
  <span class="muted">{rows.length} of {model.feed.length}</span>
  <input
    id="board-filter"
    class="push"
    type="search"
    placeholder="Filter  /"
    aria-label="Filter feed by type, name, hash or actor"
    bind:value={query}
  />
</div>
{#if types.length}<div class="filters" role="group" aria-label="Event types">
    {#each types as type (type)}<button
        type="button"
        class="toggle"
        aria-pressed={!hidden.includes(type)}
        onclick={() => toggle(type)}>{type.replaceAll("_", " ")}</button
      >{/each}
  </div>{/if}
<div class="list" data-pane="feed" role="list" aria-label="Journal events, newest first">
  {#each rows as { row, summary } (row.seq)}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="row"
      role="listitem"
      tabindex="0"
      data-row
      data-testid="board-event"
      data-type={row.type}
      data-seq={row.seq}
    >
      <span class="time numeric">{time(row.ts)}</span>
      <span class="type">{row.type.replaceAll("_", " ")}</span>
      {#if summary.name}<span class="name">{summary.name}</span
        >{:else if summary.hash}<Hash value={summary.hash} />{/if}
      {#if summary.outcome !== null}<span
          class="outcome"
          class:ok={summary.outcome === "ok"}
          class:bad={summary.outcome !== "ok"}>{summary.outcome}</span
        >{/if}
      {#if summary.elapsedMs !== null}<span class="numeric muted"
          >{summary.elapsedMs} ms</span
        >{/if}
      {#if row.type === "actor_message" && summary.actor !== null}<span
          class="actor">{summary.actor}</span
        >{#if summary.cursor !== null}<span class="numeric muted"
            >cursor {summary.cursor}</span
          >{/if}{/if}
      <span class="seq numeric muted">{row.seq}</span>
    </div>
  {:else}
    <div class="empty">
      {model.feed.length ? "Every event is filtered out." : "No events yet."}
    </div>
  {/each}
</div>

<style>
  input {
    max-width: 180px;
    padding: 3px 7px;
    font-size: 0.92em;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    padding: 5px 10px;
    border-bottom: 1px solid var(--line);
    background: var(--side);
  }
  .toggle {
    font-size: 0.86em;
    padding: 1px 7px;
    border: 1px solid var(--line);
    border-radius: 4px;
    color: var(--muted);
  }
  .toggle[aria-pressed="true"] {
    color: var(--ink);
    background: var(--card);
    border-color: var(--focus);
  }
  .list {
    flex: 1;
    min-height: 0;
    overflow: auto;
    font-size: 0.95em;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 9px;
    min-height: 26px;
    padding: 2px 10px;
    border-bottom: 1px solid var(--line);
    outline: none;
    white-space: nowrap;
  }
  .row:hover {
    background: var(--code);
  }
  .row:focus-visible {
    background: var(--selection);
  }
  .time {
    color: var(--muted);
  }
  .type {
    min-width: 108px;
    color: var(--muted);
  }
  .name {
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .outcome {
    font: 0.86em var(--mono);
    border: 1px solid var(--line);
    border-radius: 3px;
    padding: 0 4px;
  }
  .outcome.ok {
    color: var(--verdict-green);
    border-color: var(--verdict-green);
  }
  .outcome.bad {
    color: var(--verdict-red);
    border-color: var(--verdict-red);
  }
  .actor {
    font-family: var(--mono);
    font-size: 0.9em;
  }
  .seq {
    margin-left: auto;
    font-size: 0.86em;
  }
</style>

<script lang="ts">
  import { tick } from "svelte";
  import { History, RotateCcw, Check, X, ChevronRight, ChevronDown } from "lucide-svelte";
  import CodeBlock from "../CodeBlock.svelte";
  import {
    invocationSummary,
    parameterSummary,
    resultSummary,
    preview,
    type Journal,
    type JournalEntry,
  } from "./journal";
  export let journal: Journal;
  export let rerun: (entry: JournalEntry) => void;
  let list: HTMLDivElement;
  /** The pane starts as one line; the rows appear on "show". */
  let open = false;
  /** Rows whose parameters and raw input/result are shown. */
  let details = new Set<number>();
  let count = 0;
  $: storageError = journal.storageError;
  function exportHistory() {
    const url = URL.createObjectURL(
      new Blob([journal.exportJson()], { type: "application/json" }),
    );
    const link = document.createElement("a");
    link.href = url;
    link.download = "repl-history.json";
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }
  function toggleDetails(id: number) {
    const next = new Set(details);
    next.has(id) ? next.delete(id) : next.add(id);
    details = next;
  }
  $: if ($journal.length !== count) {
    count = $journal.length;
    void tick().then(() => {
      if (list) list.scrollTop = list.scrollHeight;
    });
  }
  /** The one human line for a row: what ran, and what came back. */
  function summary(entry: JournalEntry): string {
    if (entry.error) return preview(entry.error);
    if (entry.state === "running") return "Running…";
    return entry.command === "run"
      ? `${invocationSummary(entry)} ${resultSummary(entry)}`
      : resultSummary(entry);
  }
</script>

<section
  class="repl-history"
  class:open
  data-pane="history"
  aria-label="REPL history"
>
  <div class="section-bar">
    <button
      class="toggle"
      data-testid="history-toggle"
      aria-expanded={open}
      on:click={() => (open = !open)}
    >
      <History size={14} class="icon-item" />
      <h2>{$journal.length} command{$journal.length === 1 ? "" : "s"}</h2>
      <span class="muted">· {open ? "hide" : "show"}</span>
    </button>
    {#if open}
      <button class="push" on:click={exportHistory} disabled={!$journal.length}>Export</button>
      <button
        on:click={() => journal.clear()}
        disabled={!$journal.length ||
          $journal.some((entry) => entry.state === "running")}>Clear</button
      >
    {/if}
  </div>
  {#if open}
    {#if $storageError}<p class="error" role="status">{$storageError}</p>{/if}
    <div class="history-scroll" bind:this={list}>
      {#each $journal as entry (entry.id)}
        {@const parameters = parameterSummary(entry)}
        <article data-testid="history-row" data-command={entry.command}>
          <div class="entry-header">
            <button
              data-row
              class="entry"
              on:click={() => toggleDetails(entry.id)}
              aria-expanded={details.has(entry.id)}
              on:keydown={(event) => {
                if (
                  event.key === "r" &&
                  !event.metaKey &&
                  !event.ctrlKey &&
                  entry.state !== "running"
                ) {
                  event.preventDefault();
                  rerun(entry);
                }
              }}
            >
              {#if entry.state === "completed"}<Check
                  size={12}
                  class="icon-run"
                />{:else if entry.state === "failed"}<X
                  size={12}
                  class="failed"
                />{:else}<span class="running-dot"></span>{/if}
              <strong>{entry.name}</strong><time
                datetime={entry.startedAt}
                title={entry.startedAt}
                >{new Date(entry.startedAt).toLocaleTimeString()}</time
              >
              <span class="summary" class:error-text={!!entry.error}>{summary(entry)}</span>
            </button>
            <button
              class="details-toggle"
              aria-label={`${details.has(entry.id) ? "Hide" : "Show"} details of command ${entry.id}`}
              aria-expanded={details.has(entry.id)}
              data-testid="history-details"
              on:click={() => toggleDetails(entry.id)}
              >{#if details.has(entry.id)}<ChevronDown size={12} />{:else}<ChevronRight
                  size={12}
                />{/if}details</button
            >
            <button
              aria-label={`Rerun command ${entry.id}: ${entry.name}`}
              disabled={entry.state === "running"}
              on:click={() => rerun(entry)}><RotateCcw size={12} /></button
            >
          </div>
          {#if details.has(entry.id)}
            <div class="entry-detail">
              {#if parameters}<div class="parameters muted" data-testid="history-parameters">
                  {parameters}
                </div>{/if}
              <div class="io">
                <div>
                  <h3>Input</h3>
                  <CodeBlock
                    language="json"
                    code={JSON.stringify(entry.values, null, 2)}
                  />
                </div>
                <div>
                  <h3>Result</h3>
                  {#if entry.error}<p class="error">
                      {entry.error}
                    </p>{:else if entry.state === "running"}<p class="muted">
                      Running…
                    </p>{:else}<CodeBlock
                      language="json"
                      code={JSON.stringify(entry.result, null, 2)}
                    />{/if}
                </div>
              </div>
            </div>
          {/if}
        </article>
      {:else}<p class="empty">Run a command to start your history.</p>{/each}
    </div>
  {/if}
</section>

<style>
  .repl-history {
    border-top: 1px solid var(--line);
    background: var(--side);
    flex: none;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .repl-history.open {
    flex: 0 0 230px;
    min-height: 100px;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 2px 0;
    text-align: left;
  }
  .toggle:hover {
    background: transparent;
    text-decoration: underline;
  }
  .history-scroll {
    overflow: auto;
    min-height: 0;
  }
  .entry-header {
    display: flex;
    align-items: center;
    border-bottom: 1px solid var(--line);
    padding-right: 8px;
  }
  .entry {
    display: flex;
    align-items: center;
    gap: 9px;
    flex: 1;
    min-width: 0;
    padding: 6px 12px;
    text-align: left;
  }
  .entry strong {
    font-weight: 500;
    flex: none;
  }
  time {
    color: var(--muted);
    font-size: 0.85em;
    flex: none;
  }
  .summary {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--muted);
  }
  .summary.error-text {
    color: var(--error);
  }
  .running-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    border: 1px solid var(--muted);
  }
  .details-toggle {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    color: var(--muted);
    font-size: 0.9em;
  }
  .entry-detail {
    border-bottom: 1px solid var(--line);
  }
  .parameters {
    padding: 4px 12px;
    font-family: var(--mono);
    font-size: 0.9em;
    overflow-wrap: anywhere;
    border-bottom: 1px solid var(--line);
  }
  .io {
    display: grid;
    grid-template-columns: 1fr 1fr;
  }
  .io > div {
    min-width: 0;
    border-right: 1px solid var(--line);
  }
  h3 {
    padding: 6px 12px 0;
    color: var(--muted);
    font-size: 0.9em;
  }
  :global(.failed) {
    color: var(--verdict-red);
  }
</style>

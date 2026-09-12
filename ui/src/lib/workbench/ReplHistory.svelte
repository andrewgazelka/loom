<script lang="ts">
  import { tick } from "svelte";
  import { History, RotateCcw, Check, X } from "lucide-svelte";
  import CodeBlock from "../CodeBlock.svelte";
  import {
    invocationSummary,
    resultSummary,
    preview,
    type Journal,
    type JournalEntry,
  } from "./journal";
  export let journal: Journal;
  export let rerun: (entry: JournalEntry) => void;
  let list: HTMLDivElement;
  let expanded: number | null = null;
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
  $: if ($journal.length !== count) {
    count = $journal.length;
    expanded = null;
    void tick().then(() => {
      if (list) list.scrollTop = list.scrollHeight;
    });
  }
</script>

<section class="repl-history" data-pane="history" aria-label="REPL history">
  <div class="section-bar">
    <History size={14} class="icon-item" />
    <h2>REPL history</h2>
    <span class="muted">{$journal.length} commands</span>
    <button on:click={exportHistory} disabled={!$journal.length}>Export</button>
    <button
      on:click={() => journal.clear()}
      disabled={!$journal.length ||
        $journal.some((entry) => entry.state === "running")}>Clear</button
    >
  </div>
  {#if $storageError}<p class="error" role="status">{$storageError}</p>{/if}
  <div class="history-scroll" bind:this={list}>
    {#each $journal as entry (entry.id)}
      <article>
        <div class="entry-header">
          <button
            data-row
            class="entry"
            aria-expanded={expanded === entry.id}
            on:click={() =>
              (expanded = expanded === entry.id ? null : entry.id)}
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
            <span class="sequence">{entry.id.toString().padStart(2, "0")}</span>
            {#if entry.state === "completed"}<Check
                size={12}
                class="icon-run"
              />{:else if entry.state === "failed"}<X
                size={12}
                class="failed"
              />{/if}
            <strong>{entry.name}</strong><time
              datetime={entry.startedAt}
              title={entry.startedAt}
              >{new Date(entry.startedAt).toLocaleTimeString()}</time
            ><span class="muted">{entry.state}</span>
          </button>
          <button
            aria-label={`Rerun command ${entry.id}: ${entry.name}`}
            disabled={entry.state === "running"}
            on:click={() => rerun(entry)}><RotateCcw size={12} /></button
          >
        </div>
        {#if expanded !== entry.id}<div class="result-preview">
            <div class="invocation">{invocationSummary(entry)}</div>
            {#if entry.error}<span class="error">{preview(entry.error)}</span
              >{:else if entry.state === "running"}<span class="muted"
                >Running…</span
              >{:else}<span>{resultSummary(entry)}</span>{/if}
          </div>{/if}
        {#if expanded === entry.id}
          <div class="entry-detail">
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
        {/if}
      </article>
    {:else}<p class="empty">Run a command to start your history.</p>{/each}
  </div>
</section>

<style>
  .repl-history {
    border-top: 1px solid var(--line);
    background: var(--side);
    flex: 0 0 230px;
    min-height: 100px;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .history-scroll {
    overflow: auto;
    min-height: 0;
  }
  .result-preview {
    padding: 3px 12px 7px 43px;
    max-height: 45px;
    overflow: auto;
    border-bottom: 1px solid var(--line);
  }
  .entry-header {
    display: flex;
    border-bottom: 1px solid var(--line);
    padding-right: 8px;
  }
  .entry {
    display: flex;
    align-items: center;
    gap: 9px;
    flex: 1;
    padding: 7px 12px;
    text-align: left;
  }
  time {
    color: var(--muted);
    font-size: 0.85em;
  }
  .invocation {
    color: var(--muted);
  }
  .entry strong {
    font-weight: 500;
  }
  .sequence {
    font-family: var(--mono);
    color: var(--muted);
  }
  .entry-detail {
    display: grid;
    grid-template-columns: 1fr 1fr;
  }
  .entry-detail > div {
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

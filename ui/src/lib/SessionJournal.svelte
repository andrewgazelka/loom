<script lang="ts">
  import { Search } from "lucide-svelte";
  import { record, type LogEvent, type Client } from "./api";
  import { eventRow, localRow, groupDefinitions, type Entry } from "./journal";
  import EventCard from "./EventCard.svelte";
  export let client: Client;
  export let events: LogEvent[] = [];
  export let entries: Entry[] = [];
  export let inspect: (hash: string) => void;
  export let loadSource: (hash: string) => Promise<string>;
  let filter = "all",
    query = "";
  function recordedSequences(entries: Entry[], events: LogEvent[]) {
    const sequences = new Set<number>();
    for (const entry of entries) {
      if (!entry.reply?.seq) continue;
      sequences.add(entry.reply.seq);
      if (entry.mode === "eval") {
        const event = [...events]
          .reverse()
          .find(
            (event) =>
              event.seq <= entry.reply!.seq &&
              record(event.event).type === "evaluated" &&
              record(event.event).source === entry.source,
          );
        if (event) sequences.add(event.seq);
      }
    }
    return sequences;
  }
  $: localSequences = recordedSequences(entries, events);
  function primary(row: ReturnType<typeof eventRow>): boolean {
    if (row.entry) return true;
    if (row.kind === "defined")
      return (
        !!record(row.metadata).name &&
        !String(record(row.metadata).name).startsWith("session/")
      );
    return ["evaluated", "effect"].includes(
      row.kind,
    );
  }
  let limit = 20;
  $: rows = [
    ...events.filter((event) => !localSequences.has(event.seq)).map(eventRow),
    ...entries.map(localRow),
  ].sort(
    (a, b) =>
      (a.seq ?? Number.MAX_SAFE_INTEGER) - (b.seq ?? Number.MAX_SAFE_INTEGER),
  );
  $: grouped = groupDefinitions(rows);
  $: visible = (filter === "system" ? rows : grouped).filter(
    (row) =>
      ((filter === "all" && primary(row)) ||
        filter === "system" ||
        (filter === "definitions" &&
          (row.kind === "defined" || row.kind.startsWith("define")))) &&
      JSON.stringify(row).toLowerCase().includes(query.toLowerCase()),
  );
</script>

<div class="toolbar">
  <div class="tabs" aria-label="Filter journal">
    <button aria-pressed={filter === "all"} on:click={() => (filter = "all")}
      >Journal <span>{grouped.filter(primary).length}</span></button
    ><button
      aria-pressed={filter === "definitions"}
      on:click={() => (filter = "definitions")}>Definitions</button
    ><button
      aria-pressed={filter === "system"}
      on:click={() => (filter = "system")}>All events</button
    >
  </div>
  <label class="search"
    ><Search size={13} /><input
      aria-label="Find in journal"
      bind:value={query}
      placeholder="Find in journal"
      type="search"
    /></label
  >
</div>
<div class="journal" aria-live="polite">
  {#if visible.length > limit}<button
      class="earlier"
      on:click={() => (limit += 30)}
      >Show {Math.min(30, visible.length - limit)} earlier events</button
    >{/if}
  {#each visible.slice(-limit) as row (row.id)}<EventCard
    {client}
      {row}
      {inspect}
      {loadSource}
    />{:else}<div class="empty">
      {rows.length
        ? "No matching events."
        : "Your session starts here. Run an expression or define a function below."}
    </div>{/each}
</div>

<style>
  .earlier {
    display: block;
    margin: 0 auto 20px;
    padding: 9px 14px;
    font-size: 10px;
    color: var(--muted);
    border: 1px solid var(--line);
    border-radius: 5px;
  }
</style>

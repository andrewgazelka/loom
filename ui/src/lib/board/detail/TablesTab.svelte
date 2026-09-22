<script lang="ts">
  /**
   * The actor's SQLite tables through the read-only `sql` verb: names from `sqlite_master`, a
   * row count per table, and the last 50 rows of the table you click.
   */
  import type { BoardClient } from "../connect";
  import { rows as parseRows, type Row } from "../../workbench/schema";
  import RowsTable from "./RowsTable.svelte";
  import { fetched } from "../fetched.svelte";
  import { identifier, reason } from "./format";
  let { id, client }: { id: string; client: BoardClient | null } = $props();

  interface Table {
    name: string;
    count: number | null;
    countError: string | null;
  }
  type Count = { count: number; countError: null } | { count: null; countError: string };
  let chosen = $state<string | null>(null);
  let rows = $state.raw<Row[] | null>(null);
  let rowsError = $state<string | null>(null);

  async function sql(owner: BoardClient, actor: string, query: string): Promise<Row[]> {
    return parseRows(await owner.command("sql", { id: actor, query }), "sql");
  }

  // Row counts land one table at a time, keyed by actor and table so a count for another
  // actor (or a late one) never shows under this one.
  let counts = $state<Record<string, Count>>({});
  const countKey = (actor: string, name: string) => `${actor}\n${name}`;
  async function count(owner: BoardClient, actor: string, name: string) {
    let next: Count;
    try {
      const [row] = await sql(owner, actor, `SELECT COUNT(*) AS n FROM ${identifier(name)}`);
      if (!row || typeof row.n !== "number") throw new Error("COUNT(*) returned no number");
      next = { count: row.n, countError: null };
    } catch (problem) {
      next = { count: null, countError: reason(problem) };
    }
    counts = { ...counts, [countKey(actor, name)]: next };
  }
  const loaded = fetched(
    () => [client, id],
    (owner, actor) =>
      owner === null
        ? null
        : sql(owner, actor, "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").then(
            (listed): string[] => {
              const names = listed.map((row) => {
                if (typeof row.name !== "string")
                  throw new Error("sqlite_master row without a name");
                return row.name;
              });
              for (const name of names) void count(owner, actor, name);
              return names;
            },
          ),
  );
  const error = $derived(loaded.error);
  const tables = $derived<Table[] | null>(
    loaded.value === null
      ? null
      : loaded.value.map((name) => ({
          name,
          ...(counts[countKey(id, name)] ?? { count: null, countError: null }),
        })),
  );

  function open(name: string) {
    const owner = client;
    if (owner === null) return;
    chosen = name;
    rows = null;
    rowsError = null;
    sql(owner, id, `SELECT * FROM ${identifier(name)} ORDER BY rowid DESC LIMIT 50`).then(
      (loaded) => {
        if (client === owner && chosen === name) rows = loaded;
      },
      (problem: unknown) => {
        if (client === owner && chosen === name) rowsError = reason(problem);
      },
    );
  }
</script>

{#if client === null}
  <div class="note">Not connected.</div>
{:else if error !== null}
  <div class="error" role="alert">{error}</div>
{:else if tables === null}
  <div class="note">Reading tables of {id}…</div>
{:else}
  <div class="layout">
    <div class="tables" role="listbox" aria-label={`Tables of ${id}`}>
      {#each tables as table (table.name)}
        <div
          class="table-row"
          role="option"
          tabindex="0"
          data-row
          data-table={table.name}
          aria-selected={chosen === table.name}
          onclick={() => open(table.name)}
          onkeydown={(event) => {
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              open(table.name);
            }
          }}
        >
          <span class="table-name">{table.name}</span>
          {#if table.countError !== null}<span class="error-text" title={table.countError}
              >count failed</span
            >{:else if table.count === null}<span class="muted">…</span
            >{:else}<span class="numeric muted">{table.count} rows</span>{/if}
        </div>
      {:else}
        <div class="empty">The actor has no tables.</div>
      {/each}
    </div>
    <div class="rows">
      {#if chosen === null}
        <div class="note">Click a table for its last 50 rows.</div>
      {:else if rowsError !== null}
        <div class="error" role="alert">{rowsError}</div>
      {:else if rows === null}
        <div class="note">Reading {chosen}…</div>
      {:else}
        <div class="section-bar">
          <h3>{chosen}</h3>
          <span class="muted">last {rows.length} rows</span>
        </div>
        <RowsTable {rows} label={`Rows of ${chosen}`} />
      {/if}
    </div>
  </div>
{/if}

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(180px, 240px) minmax(0, 1fr);
    flex: 1;
    min-height: 0;
  }
  .tables {
    border-right: 1px solid var(--line);
    overflow: auto;
  }
  .table-row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 28px;
    padding: 2px 10px;
    border-bottom: 1px solid var(--line);
    cursor: pointer;
    outline: none;
  }
  .table-row:hover {
    background: var(--code);
  }
  .table-row[aria-selected="true"] {
    background: var(--selection);
  }
  .table-row:focus-visible {
    outline: 1px solid var(--focus);
    outline-offset: -1px;
  }
  .table-name {
    font-family: var(--mono);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rows {
    overflow: auto;
    min-width: 0;
  }
  .error-text {
    color: var(--error);
    font-size: 0.9em;
  }
</style>

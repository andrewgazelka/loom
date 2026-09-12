<script lang="ts">
  import CodeBlock from "../CodeBlock.svelte";
  import Hash from "./Hash.svelte";
  import type { Json, Row } from "./schema";
  export let rows: Row[];
  export let label: string;
  export let columns: string[] = [];
  let selected: number | null = null;
  $: keys = columns.length
    ? columns
    : [...new Set(rows.flatMap((row) => Object.keys(row)))];
  function display(value: Json | undefined): string {
    if (value === undefined) return "—";
    return typeof value === "string" ? value : JSON.stringify(value);
  }
</script>

<div class="table-wrap">
  <table aria-label={label}>
    <thead
      ><tr
        >{#each keys as key}<th>{key.replaceAll("_", " ")}</th>{/each}<th
          class="narrow">Inspect</th
        ></tr
      ></thead
    >
    <tbody
      >{#each rows as row, index}<tr class:selected={selected === index}>
          {#each keys as key}<td
              >{#if typeof row[key] === "string" && /^[a-f0-9]{64}$/.test(String(row[key]))}<Hash
                  value={String(row[key])}
                />{:else}<div class="cell" title={display(row[key])}>
                  <CodeBlock
                    inline
                    language={key === "query" ? "sql" : "json"}
                    code={key === "query" && typeof row[key] === "string"
                      ? String(row[key])
                      : row[key] === undefined
                        ? "null"
                        : JSON.stringify(row[key])}
                  />
                </div>{/if}</td
            >{/each}
          <td
            ><button
              data-row
              class="text-control"
              aria-label={`Inspect ${label} row ${index + 1}`}
              on:click={() => (selected = selected === index ? null : index)}
              >Open</button
            ></td
          >
        </tr>{:else}<tr
          ><td colspan={keys.length + 1} class="empty">No rows returned.</td
          ></tr
        >{/each}</tbody
    >
  </table>
</div>
{#if selected !== null && rows[selected]}<section class="row-inspector">
    <div class="section-bar">
      <h3>Row {selected + 1}</h3>
      <button on:click={() => (selected = null)}>Close</button>
    </div>
    <CodeBlock language="json" code={JSON.stringify(rows[selected], null, 2)} />
  </section>{/if}

<style>
  .cell {
    max-width: 34rem;
    max-height: 5em;
    overflow: auto;
  }
  .row-inspector {
    border-top: 1px solid var(--line);
  }
  .narrow {
    width: 60px;
  }
</style>

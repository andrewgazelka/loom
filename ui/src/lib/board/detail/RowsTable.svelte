<script lang="ts">
  /** A plain table over `sql`-shaped rows: 64-hex strings shorten to eight characters, objects print as JSON. */
  import type { Row } from "../../workbench/schema";
  import { HASH, short } from "./format";
  let {
    rows,
    label,
    columns = [],
  }: { rows: Row[]; label: string; columns?: string[] } = $props();
  const keys = $derived(
    columns.length ? columns : [...new Set(rows.flatMap((row) => Object.keys(row)))],
  );
  function cell(value: unknown): { text: string; hash: boolean } {
    if (value === undefined || value === null) return { text: "—", hash: false };
    if (typeof value === "string")
      return HASH.test(value) ? { text: short(value), hash: true } : { text: value, hash: false };
    return { text: JSON.stringify(value), hash: false };
  }
</script>

<div class="table-wrap">
  <table aria-label={label}>
    <thead><tr>{#each keys as key (key)}<th>{key.replaceAll("_", " ")}</th>{/each}</tr></thead>
    <tbody>
      {#each rows as row, index (index)}
        <tr>
          {#each keys as key (key)}
            {@const shown = cell(row[key])}
            <td class:hash={shown.hash} title={shown.hash ? String(row[key]) : undefined}
              >{shown.text}</td
            >
          {/each}
        </tr>
      {:else}
        <tr><td colspan={Math.max(1, keys.length)} class="empty">No rows.</td></tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  td {
    font-family: var(--mono);
    font-size: 0.92em;
    max-width: 36rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  td.hash {
    color: var(--muted);
  }
</style>

<script lang="ts">
  /** Behavior history of the actor from the `lineage` verb. */
  import type { BoardClient } from "../connect";
  import { rows as parseRows, type Row } from "../../workbench/schema";
  import RowsTable from "./RowsTable.svelte";
  import { reason } from "./format";
  let { id, client }: { id: string; client: BoardClient | null } = $props();
  let rows = $state.raw<Row[] | null>(null);
  let error = $state<string | null>(null);
  $effect(() => {
    const owner = client;
    const actor = id;
    rows = null;
    error = null;
    if (owner === null) return;
    owner.command("lineage", { id: actor }).then(
      (result) => {
        if (client !== owner || id !== actor) return;
        try {
          rows = parseRows(result, "lineage");
        } catch (problem) {
          error = reason(problem);
        }
      },
      (problem: unknown) => {
        if (client === owner && id === actor) error = reason(problem);
      },
    );
  });
</script>

{#if client === null}
  <div class="note">Not connected.</div>
{:else if error !== null}
  <div class="error" role="alert">{error}</div>
{:else if rows === null}
  <div class="note">Reading lineage of {id}…</div>
{:else}
  <RowsTable {rows} label={`Lineage of ${id}`} />
{/if}

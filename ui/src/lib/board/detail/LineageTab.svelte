<script lang="ts">
  /** Behavior history of the actor from the `lineage` verb. */
  import type { BoardClient } from "../connect";
  import { rows as parseRows, type Row } from "../../workbench/schema";
  import RowsTable from "./RowsTable.svelte";
  import { fetched } from "../fetched.svelte";
  let { id, client }: { id: string; client: BoardClient | null } = $props();
  const loaded = fetched(
    () => [client, id],
    (owner, actor) =>
      owner === null
        ? null
        : owner
            .command("lineage", { id: actor })
            .then((result): Row[] => parseRows(result, "lineage")),
  );
  const rows = $derived(loaded.value);
  const error = $derived(loaded.error);
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

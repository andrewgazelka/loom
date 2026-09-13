<script lang="ts">
  import { V } from "./commands";
  import { repairFields, updateSession, type Json } from "./schema";
  import CodeBlock from "../CodeBlock.svelte";
  import DataTable from "./DataTable.svelte";
  import Hash from "./Hash.svelte";
  export let value: Json;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
  $: session = updateSession(value);
  $: fields = repairFields(session);
  $: repairable = ["pending", "needs_repair"].includes(session.status);
</script>

<section aria-label="Update session">
  <div class="section-bar">
    <h2>Update {session.target}</h2>
    <span role="status">{session.status.replaceAll("_", " ")}</span>
    <span class="push muted">Revision {session.revision}</span>
  </div>
  <div class="section-bar">
    <Hash value={session.id} />
    <button
      class="text-control"
      on:click={() => navigate(V.update_view, { id: session.id })}
      >Inspect session</button
    >
    {#if session.status === "conflict"}
      <button
        class="text-control"
        on:click={() => navigate(V.update_rebase, fields)}>Rebase update</button
      >
    {/if}
    {#if repairable}
      <button
        class="text-control"
        on:click={() => navigate(V.update_repair, fields)}
        >Repair sources</button
      >
    {/if}
    {#if !["complete", "aborted"].includes(session.status)}
      <button
        class="text-control"
        on:click={() => navigate(V.update_abort, fields)}>Abort update…</button
      >
    {/if}
  </div>
  {#if session.status === "needs_repair"}<p class="note">
      The compiler needs source changes before this update can publish. Repair
      sources opens the affected definitions as an editable JSON batch.
    </p>
  {:else if session.status === "conflict"}<p class="note">
      The namespace changed while this update was being built. Rebase retries
      against current definitions if your edited definitions are unchanged. If
      another update changed an edited definition, start a new update from its
      current hash.
    </p>
  {:else if session.status === "complete"}<p class="note">
      The update and its affected callers were published together.
    </p>{/if}
  <DataTable
    label="Propagated definitions"
    rows={session.changes.map((change) => ({ ...change }))}
  />
  {#each session.diagnostics as diagnostic}
    <section
      aria-label={`Repair ${diagnostic.names.join(", ") || diagnostic.hash}`}
    >
      <div class="section-bar">
        <h3>{diagnostic.names.join(", ") || "Unnamed definition"}</h3>
        <Hash value={diagnostic.hash} />
      </div>
      <CodeBlock
        code={JSON.stringify(diagnostic.diagnostics, null, 2)}
        language="json"
      />
      <CodeBlock code={diagnostic.source} language="rust" />
    </section>
  {/each}
</section>

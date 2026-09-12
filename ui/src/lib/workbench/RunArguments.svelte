<script lang="ts">
  import { onDestroy } from "svelte";
  import { WorkbenchClient, RequestSlot } from "./client";
  import { commandById, V } from "./commands";
  import { argumentHints, type ArgumentHints } from "./runArguments";
  export let client: WorkbenchClient;
  export let target: string;
  const slot = new RequestSlot();
  let hints: ArgumentHints | null = null;
  let error = "";
  let loading = false;
  function load(client: WorkbenchClient, target: string) {
    slot.cancel();
    hints = null;
    error = "";
    loading = Boolean(target);
    if (!target) return;
    void slot.run(
      async (signal) =>
        argumentHints(
          await client.call(commandById(V.view), { target }, signal),
        ),
      (value) => (hints = value),
      (message) => (error = message),
      () => (loading = false),
    );
  }
  $: load(client, target.trim());
  onDestroy(() => slot.cancel());
</script>

<aside aria-label="Run argument hints">
  {#if loading}<p role="status">Loading signature…</p>
  {:else if error}<p role="status">Signature unavailable: {error}</p>
  {:else if hints}
    {#each hints.signatures as signature}<code>{signature}</code>{/each}
    <p>{hints.note}</p>
    {#if hints.example !== null}<p>Example arguments JSON</p>
      <pre>{hints.example}</pre>{/if}
  {:else}<p>Enter a target to see its signature and example arguments.</p>{/if}
</aside>

<style>
  aside {
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 0.9em;
    color: var(--muted);
  }
  code {
    display: block;
    overflow-wrap: anywhere;
  }
  p {
    margin: 6px 0;
  }
  pre {
    margin: 6px 0;
    white-space: pre-wrap;
  }
</style>

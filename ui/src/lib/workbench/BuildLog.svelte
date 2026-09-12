<script lang="ts">
  import { onDestroy } from "svelte";
  import type { WorkbenchClient } from "./client";
  import { object, string, type Json } from "./schema";
  export let client: WorkbenchClient;
  export let value: Json;
  let logs: string | undefined;
  let error = "";
  let loading = false;
  let controller: AbortController | undefined;
  $: build = object(value, "definition result").build;
  $: logHash =
    build === undefined
      ? null
      : string(object(build, "build").logs_ref, "compiler log hash");
  $: {
    logHash;
    logs = undefined;
    error = "";
  }
  async function load() {
    if (!logHash || logs !== undefined || loading) return;
    loading = true;
    controller = new AbortController();
    try {
      logs = await client.compilerLog(logHash, controller.signal);
    } catch (problem) {
      if (!controller.signal.aborted) error = String(problem);
    } finally {
      loading = false;
    }
  }
  onDestroy(() => controller?.abort());
</script>

{#if logHash}<details
    on:toggle={(event) => {
      if (event.currentTarget.open) void load();
    }}
  >
    <summary>Compiler output</summary>
    {#if error}<p role="alert">{error}</p>
      <button on:click={load}>Retry compiler output</button>
    {:else if logs !== undefined}<pre>{logs ||
          "Compiler produced no output."}</pre>
    {:else}<p role="status">Loading compiler output…</p>{/if}
  </details>{/if}

<style>
  details {
    padding: 12px;
    border-top: 1px solid var(--line);
  }
  summary {
    cursor: pointer;
  }
  pre {
    font-family: var(--mono);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 360px;
    overflow: auto;
  }
</style>

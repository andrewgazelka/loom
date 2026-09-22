<script lang="ts">
  /** The raw build log from the CAS, fetched once per component (the client caches it), monospace. */
  import type { BoardClient } from "../connect";
  import { fetched } from "../fetched.svelte";
  import { short } from "./format";
  let {
    logsRef,
    componentHash,
    client,
  }: {
    logsRef: string | null;
    componentHash: string;
    client: BoardClient | null;
  } = $props();
  const loaded = fetched(
    () => [client, logsRef],
    (owner, ref) => (owner === null || ref === null ? null : owner.text(ref)),
  );
  const log = $derived(loaded.value);
  const error = $derived(loaded.error);
  const lines = $derived(log === null ? [] : log.split("\n"));
</script>

{#if logsRef === null}
  <div class="note">No build log recorded for {short(componentHash)}.</div>
{:else if client === null}
  <div class="note">Not connected.</div>
{:else if error !== null}
  <div class="error" role="alert">{error}</div>
{:else if log === null}
  <div class="note">Reading build log…</div>
{:else}
  <div class="section-bar">
    <h3>Build log</h3>
    <span class="muted">{lines.length} lines</span>
  </div>
  <pre class="log" data-testid="detail-log">{log ||
      "The build produced no output."}</pre>
{/if}

<style>
  .log {
    font: 0.9em/1.5 var(--mono);
    padding: 12px 16px;
    white-space: pre;
    overflow: auto;
    background: var(--code);
    flex: 1;
    min-height: 0;
  }
</style>

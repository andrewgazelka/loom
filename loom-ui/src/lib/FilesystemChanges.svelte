<script lang="ts">
  import { record, type Client } from "./api";
  import TextDiff from "./TextDiff.svelte";
  export let value: unknown;
  export let client: Client;
  export let inspect: (hash: string) => void;
  $: data=record(value);
  $: capture=record(data.filesystem_capture);
  $: changes=Array.isArray(data.filesystem_changes) ? data.filesystem_changes : [];
  $: unavailable=Array.isArray(capture.unavailable_paths) ? capture.unavailable_paths : [];
  let expanded: Record<string,boolean> = {};
</script>
<section class="filesystem-changes">
  <h3>{capture.preview === true ? "Filesystem preview" : "Filesystem changes"}</h3>
  {#if capture.preview === true}<p>Proposed writes; these changes were not applied to disk.</p>{/if}
  {#if capture.scope === "explicit_paths"}<p>Capture covers only the explicitly selected paths.</p>{/if}
  {#each changes as item, index}
    {@const change=record(item)}
    <details bind:open={expanded[String(index)]}><summary>{String(change.path || "File")} <span>{String(change.machine || "")}</span></summary>
      {#if expanded[String(index)]}{#if change.captured === true && (typeof change.before === "string" || change.before === null) && (typeof change.after === "string" || change.after === null)}<TextDiff {client} {inspect} before={change.before} after={change.after} contentEncoding={typeof change.content_encoding === "string" ? change.content_encoding : "raw"} />{:else}<p>Before/after content was not captured for this path.</p>{/if}{/if}
    </details>
  {/each}
  {#each unavailable as item}<p>{String(record(item).path)}: {String(record(item).reason || "Capture unavailable")}</p>{/each}
  {#if capture.unavailable_reason}<p>{String(capture.unavailable_reason)}</p>{:else if !changes.length && !unavailable.length}<p>{(capture.scope === "explicit_paths" || capture.preview === true) ? "No captured path changed." : "Before/after content was not captured. No file diff is available."}</p>{/if}
</section>
<style>
  h3{font-size:12px;font-weight:500;}p{font-size:11px;color:var(--muted);}details{border-top:1px solid var(--line);padding:10px 0;}summary{cursor:pointer;font:11px var(--mono);overflow-wrap:anywhere;}summary span{color:var(--muted);margin-left:12px;}.filesystem-changes{margin-top:18px;}
</style>

<script lang="ts">
  import { onMount } from "svelte";
  import type { Client } from "./api";
  import { diffLines, decodeText, type DiffLine } from "./text-diff";
  import ReferenceLink from "./ReferenceLink.svelte";
  export let client: Client;
  export let before: string | null;
  export let after: string | null;
  export let inspect: (hash: string) => void;
  let loading = true, error = "", binary = false;
  let lines: DiffLine[] = [], beforeSize = 0, afterSize = 0;
  onMount(() => {
    let disposed = false;
    void Promise.all([before ? client.bytes(before) : new Uint8Array(),after ? client.bytes(after) : new Uint8Array()]).then(result => {
      if(disposed) return;
      beforeSize=result[0]!.length; afterSize=result[1]!.length;
      const left=decodeText(result[0]!),right=decodeText(result[1]!);
      binary=left===null || right===null;
      if(left!==null && right!==null) lines=diffLines(left,right);
    }).catch(e => {if(!disposed)error=String(e);}).finally(() => {if(!disposed)loading=false;});
    return () => {disposed=true;};
  });
</script>
<div class="diff-links"><span>{before === null ? "Created" : after === null ? "Deleted" : "Modified"}</span>{#if before}<span>Before <ReferenceLink hash={before} {inspect} /></span>{/if}{#if after}<span>After <ReferenceLink hash={after} {inspect} /></span>{/if}</div>
{#if loading}<p>Reading captured content…</p>{:else if error}<p class="unavailable">Diff preview unavailable: {error}</p>{:else if binary}<p>Binary content · {beforeSize} bytes before → {afterSize} bytes after. Use the content links to inspect each version.</p>{:else}<div class="text-diff" aria-label="Captured text changes">{#each lines.slice(0,2000) as line}<div class:added={line.kind === "added"} class:removed={line.kind === "removed"}><span class="marker">{line.kind === "added" ? "+" : line.kind === "removed" ? "−" : " "}</span><span>{line.text || " "}</span></div>{:else}<p>Empty file.</p>{/each}</div>{#if lines.length > 2000}<p>Showing the first 2,000 diff lines. Complete versions remain available through the content links.</p>{/if}{/if}
<style>
  .diff-links {display:flex;flex-wrap:wrap;gap:12px;color:var(--muted);font:10px var(--mono);margin:12px 0;}
  p {font-size:11px;color:var(--muted);}
  .text-diff {overflow:auto;max-height:420px;border:1px solid var(--line);border-radius:5px;font:11px/1.8 var(--mono);}
  .text-diff > div {display:flex;white-space:pre;min-width:fit-content;}
  .marker {width:26px;flex:none;text-align:center;user-select:none;}
  .added {background:#2da44e20;}.removed {background:#cf222e20;}
  .added .marker {color:#26863f;}.removed .marker {color:#cf444c;}
  @media(prefers-color-scheme:dark){.added .marker{color:#7ee787;}.removed .marker{color:#ffa198;}}
</style>

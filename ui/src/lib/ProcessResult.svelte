<script lang="ts">
  import { record } from "./api";
  import CodeBlock from "./CodeBlock.svelte";
  export let value: unknown;
  $: data=record(value);
</script>
<div class="process-result">
  {#if typeof data.code === "number"}<span class="exit">Exit {data.code}</span>{/if}
  {#if typeof data.error === "string" && data.error}<p class="error">{data.error}</p>{/if}
  {#if typeof data.stdout === "string" && data.stdout}<details open><summary>Standard output</summary><CodeBlock code={data.stdout.slice(0,16384)} />{#if data.stdout.length > 16384}<p>Output preview limited to 16,384 characters.</p>{/if}</details>{/if}
  {#if typeof data.stderr === "string" && data.stderr}<details open><summary>Standard error</summary><CodeBlock code={data.stderr.slice(0,16384)} />{#if data.stderr.length > 16384}<p>Output preview limited to 16,384 characters.</p>{/if}</details>{/if}
</div>
<style>
  .process-result {margin:14px 0;font-size:11px;}
  .exit,summary,p {color:var(--muted);}
  details {margin-top:10px;}summary{cursor:pointer;margin-bottom:6px;}.error{color:var(--error);}
</style>

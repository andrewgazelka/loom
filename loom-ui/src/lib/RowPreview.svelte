<script lang="ts">
  import { onMount } from "svelte";
  import { boundedPreview } from "./preview";
  import { record } from "./api";
  import CodeBlock from "./CodeBlock.svelte";
  import ValueView from "./ValueView.svelte";
  export let load: () => Promise<unknown>;
  export let inspect: (hash: string) => void = () => {};
  export let code = false;
  export let language: "ts" | "rust" = "ts";
  let node: HTMLDivElement, value: unknown, ready = false, error = "";
  onMount(() => {
    let disposed = false;
    const observer = new IntersectionObserver(entries => {
      if (!entries.some(entry => entry.isIntersecting)) return;
      observer.disconnect();
      void boundedPreview(load).then(result => { if (!disposed) { value = result; ready = true; } }).catch(e => { if (!disposed) error = String(e); });
    }, {rootMargin:"100px"});
    observer.observe(node);
    return () => { disposed = true; observer.disconnect(); };
  });
</script>
<div class="row-preview" bind:this={node}>
  {#if ready}{#if code}<CodeBlock code={String(value || "Source unavailable").slice(0,320)} language={language === "rust" ? "rust" : "typescript"} />
    {:else}<ValueView value={record(value).type === "evaluated" ? record(value).result : value} {inspect} />{/if}
  {:else}<span class="quiet" title={error || undefined}>{error ? "Preview unavailable" : "Loading preview…"}</span>{/if}
</div>
<style>
  .row-preview { margin:8px 0; max-height:130px; overflow:auto; font-size:10px; }
  .row-preview :global(pre) { padding:8px 10px; white-space:pre-wrap; }
</style>

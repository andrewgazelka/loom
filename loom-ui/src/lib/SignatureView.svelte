<script lang="ts">
  import EffectBadges from "./EffectBadges.svelte";
  import { record } from "./api";
  import { signatures } from "./signature";
  import CodeBlock from "./CodeBlock.svelte";
  export let value: unknown;
  export let name = "";
  $: exports = Array.isArray(record(value).exports) ? record(value).exports as unknown[] : [];
</script>
<div class="signature-view">
  {#each signatures(value, name) as signature, index}<CodeBlock code={signature} language="typescript" /><EffectBadges inferred={record(exports[index]).effects} />{/each}
</div>
<style>
  .signature-view { min-width:0; }
  .signature-view :global(pre) { padding:3px 0; background:transparent !important; white-space:pre-wrap; font-size:12px; }
  .signature-view :global(.code-block) { max-height:none; }
</style>

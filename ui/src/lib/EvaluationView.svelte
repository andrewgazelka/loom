<script lang="ts">
  import { record, type Client } from "./api";
  import FilesystemChanges from "./FilesystemChanges.svelte";
  export let client: Client;
  $: preview = record(value);
  $: isPreview = record(preview.filesystem_capture).preview === true && Array.isArray(preview.filesystem_changes);
  import CodeBlock from "./CodeBlock.svelte";
  import ValueView from "./ValueView.svelte";
  import ReferenceLink from "./ReferenceLink.svelte";
  export let value: unknown;
  export let source = "";
  export let definition = "";
  export let resultFirst = false;
  export let inspect: (hash: string) => void;
</script>

<div class="evaluation" class:result-first={resultFirst}>
  {#if source && !resultFirst}<div class="evaluation-source">
      <CodeBlock code={source} language="typescript" />
    </div>{/if}
  <div class="evaluation-value"><ValueView value={isPreview ? preview.result : value} {inspect} />{#if isPreview}<FilesystemChanges {value} {client} {inspect} />{/if}</div>
  {#if source && resultFirst}<div class="evaluation-source">
      <CodeBlock code={source} language="typescript" />
    </div>{/if}{#if definition}<div class="evaluation-definition">
      <span>Definition</span><ReferenceLink hash={definition} {inspect} />
    </div>{/if}
</div>

<style>
  .evaluation-source {
    background: var(--card);
  }
  .evaluation-source :global(pre) {
    background: var(--card) !important;
  }
  .evaluation-value {
    padding: 15px 18px;
    background: var(--code);
    border-top: 1px solid var(--line);
  }
  .evaluation-value :global(.value-line) {
    font-size: 13px;
  }
  .result-first .evaluation-value {
    border-top: 0;
    background: var(--card);
    padding: 18px 20px;
  }
  .result-first .evaluation-value :global(.value-line) {
    font-size: 17px;
  }
  .result-first .evaluation-source {
    border-top: 1px solid var(--line);
  }
  .result-first .evaluation-source :global(pre) {
    background: var(--code) !important;
  }
  .evaluation-definition {
    display: flex;
    gap: 8px;
    padding: 12px 20px;
    font: 10px/1.8 var(--mono);
    color: var(--muted);
    border-top: 1px solid var(--line);
    overflow-wrap: anywhere;
  }
</style>

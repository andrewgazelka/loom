<script lang="ts">
  import { displayNumber } from "./number";
  import { record, cid } from "./api";
  import ObjectFields from "./ObjectFields.svelte";
  import ArrayView from "./ArrayView.svelte";
  import ReferenceLink from "./ReferenceLink.svelte";
  export let value: unknown;
  export let label = "";
  export let depth = 0;
  export let inspect: (hash: string) => void;
  let expanded = depth < 2;
  $: object = record(value);
  $: reference = typeof object.$ref === "string" ? object.$ref : null;
  $: expandable = !reference && value !== null && typeof value === "object";
  $: count = Array.isArray(value) ? value.length : Object.keys(object).length;
</script>

{#if reference}<span class="value-line"
    >{#if label}<span class="value-key">{label}: </span>{/if}<ReferenceLink
      hash={reference}
      {inspect}
    /></span
  >{:else if expandable}<details class="value-tree" bind:open={expanded}>
    <summary
      >{#if label}<span class="value-key">{label}</span>{/if}<span
        class="value-shape"
        >{Array.isArray(value) ? `[${count} items]` : `{${count} fields}`}</span
      ></summary
    >{#if expanded}{#if Array.isArray(value)}<ArrayView
        {value}
        {depth}
        {inspect}
      />{:else}<ObjectFields value={object} {depth} {inspect} />{/if}{/if}
  </details>{:else}<div class="value-line">
    {#if label}<span class="value-key"
        >{label}:
      </span>{/if}{#if typeof value === "string" && cid(value)}<ReferenceLink
        hash={value}
        {inspect}
      />{:else}<span
        >{typeof value === "string"
          ? label
            ? JSON.stringify(value)
            : value
          : typeof value === "number"
            ? displayNumber(value)
            : String(value ?? "null")}</span
      >{/if}
  </div>{/if}

<style>
  .value-tree,
  .value-line {
    font: 11px/1.9 var(--mono);
    overflow-wrap: anywhere;
  }
  .value-tree summary {
    display: flex;
    gap: 9px;
    align-items: baseline;
    cursor: pointer;
    width: fit-content;
  }
  .value-tree summary:before {
    content: "›";
    color: var(--muted);
    display: inline-block;
    width: 8px;
  }
  .value-tree[open] > summary:before {
    transform: rotate(90deg);
  }
  .value-key {
    color: var(--muted);
  }
  .value-shape {
    color: var(--muted);
    font-size: 10px;
  }
  .value-line {
    padding: 1px 0;
  }
</style>

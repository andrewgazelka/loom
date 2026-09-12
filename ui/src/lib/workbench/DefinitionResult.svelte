<script lang="ts">
  import {
    FileCode2,
    Fingerprint,
    GitCommitHorizontal,
    Play,
    ArrowRight,
  } from "lucide-svelte";
  import CodeBlock from "../CodeBlock.svelte";
  import { diffLines } from "../text-diff";
  import {
    array,
    definition,
    definitionView,
    definitionDiff,
    history,
    object,
    rows,
    type Json,
  } from "./schema";
  import Hash from "./Hash.svelte";
  import DataTable from "./DataTable.svelte";
  export let operation: string;
  export let value: Json;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
  let selectedItem = "";
</script>

{#if ["view", "add", "update"].includes(operation)}
  {@const def = definitionView(value)}
  <div class="section-bar">
    <FileCode2 size={15} class="icon-code" />
    <h2>{def.name}</h2>
    <span class="muted">Rust</span><button
      class="push text-control"
      on:click={() => navigate("run", { hash: def.hash })}
      ><Play size={12} /> Run</button
    >
  </div>
  <div class="identity-strip">
    <span>Definition <Hash value={def.hash} /></span><span
      >Entry <Hash value={def.entry_item_hash} /></span
    ><time>{def.updated}</time>
  </div>
  <div class="source-items">
    <section class="source">
      <div class="section-bar">
        <h3>Source</h3>
        <span class="muted">{def.source.split("\n").length} lines</span>
      </div>
      <CodeBlock code={def.source} language="rust" />
    </section>
    <section class="items">
      <div class="section-bar">
        <Fingerprint size={14} class="icon-item" />
        <h3>Items</h3>
        <span class="muted">{def.items.length}</span>
      </div>
      <table aria-label="Definition items">
        <thead><tr><th>Item</th><th>Hash</th><th>Preimage</th></tr></thead
        ><tbody>
          {#each def.items as item}<tr
              class:selected={selectedItem === item.name}
              ><td
                ><button
                  data-row
                  class="text-control"
                  on:click={() => (selectedItem = item.name)}
                  ><CodeBlock inline language="rust" code={item.name} /></button
                ></td
              ><td><Hash value={item.hash} /></td><td class="numeric"
                >{item.preimage_size} B</td
              ></tr
            >{:else}<tr
              ><td colspan="3" class="empty">No item hashes returned.</td></tr
            >{/each}
        </tbody>
      </table>
      {#each def.items.filter((item) => item.name === selectedItem) as item}<div
          class="item-inspector"
        >
          <h3>{item.name}</h3>
          <Hash value={item.hash} full />
          <dl>
            <dt>Preimage size</dt>
            <dd>{item.preimage_size} bytes</dd>
            <dt>References</dt>
            <dd>{item.refs.join(", ") || "None"}</dd>
          </dl>
        </div>{/each}
    </section>
  </div>
{:else if ["find", "dependents"].includes(operation)}
  {@const definitions = array(value, operation).map(definition)}
  <div class="section-bar">
    <h2>{operation === "find" ? "Definitions" : "Dependents"}</h2>
    <span class="muted">{definitions.length}</span>
  </div>
  <div class="table-wrap">
    <table aria-label="Definitions">
      <thead
        ><tr
          ><th>Name</th><th>Hash</th><th>Entry item hash</th><th>Updated</th
          ></tr
        ></thead
      ><tbody>
        {#each definitions as def}<tr
            ><td
              ><button
                data-row
                class="text-control"
                on:click={() =>
                  navigate("view", { hash: def.hash, name: def.name })}
                ><FileCode2 size={13} class="icon-code" />{def.name}</button
              ></td
            ><td><Hash value={def.hash} /></td><td
              ><Hash value={def.entry_item_hash} /></td
            ><td>{def.updated}</td></tr
          >{:else}<tr
            ><td colspan="4" class="empty">No definitions found.</td></tr
          >{/each}
      </tbody>
    </table>
  </div>
{:else if operation === "history"}
  <div class="section-bar">
    <GitCommitHorizontal size={15} class="icon-item" />
    <h2>Hash chain</h2>
  </div>
  {#each history(value) as revision}<section class="revision">
      <div class="section-bar">
        <Hash value={revision.hash} /><ArrowRight size={12} /><Hash
          value={revision.parent_hash}
        /><time class="push muted">{revision.updated}</time><button
          data-row
          class="text-control"
          disabled={!revision.parent_hash}
          on:click={() =>
            navigate("diff", {
              before: revision.parent_hash!,
              after: revision.hash,
            })}>Diff parent</button
        >
      </div>
      <DataTable
        label="Changed items"
        rows={revision.changed_items.map((item) => ({ ...item }))}
      />
    </section>{:else}<p class="empty">No history returned.</p>{/each}
{:else if operation === "diff"}
  {@const diff = definitionDiff(value)}
  <div class="section-bar">
    <h2>Definition diff</h2>
    <Hash value={diff.before} /><ArrowRight size={12} /><Hash
      value={diff.after}
    />
  </div>
  <DataTable
    label="Changed items"
    rows={diff.changed_items.map((item) => ({ ...item }))}
  />
  <div class="diff-code" aria-label="Source diff">
    {#each diffLines(diff.source_before, diff.source_after) as line}<div
        class:added={line.kind === "added"}
        class:removed={line.kind === "removed"}
      >
        <span
          >{line.kind === "added"
            ? "+"
            : line.kind === "removed"
              ? "−"
              : " "}</span
        ><CodeBlock inline language="rust" code={line.text || " "} />
      </div>{/each}
  </div>
{:else if operation === "run"}
  {@const result = object(value, "run")}
  <div class="section-bar">
    <Play size={14} class="icon-run" />
    <h2>Output</h2>
  </div>
  <CodeBlock code={JSON.stringify(result.output, null, 2)} language="json" />
  <div class="section-bar"><h3>Effects performed</h3></div>
  <DataTable
    label="Effects performed"
    rows={rows(result.effects, "run.effects")}
  />
{/if}

<style>
  .identity-strip {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    font-size: 0.92em;
    color: var(--muted);
  }
  .identity-strip time {
    margin-left: auto;
  }
  .source-items {
    display: grid;
    grid-template-columns: minmax(280px, 1fr) minmax(290px, 0.8fr);
    min-height: 340px;
  }
  .items {
    border-left: 1px solid var(--line);
    overflow: auto;
  }
  .item-inspector {
    padding: 14px;
    border-top: 1px solid var(--line);
  }
  .item-inspector h3 {
    margin-bottom: 10px;
  }
  .revision {
    border-bottom: 1px solid var(--line);
  }
  .diff-code {
    font: 1em/1.9 var(--mono);
    overflow: auto;
    padding: 8px 0;
  }
  .diff-code > div {
    display: flex;
    white-space: pre;
    min-width: fit-content;
  }
  .diff-code span {
    width: 32px;
    text-align: center;
    flex: none;
  }
  .added {
    background: color-mix(in srgb, var(--verdict-green) 18%, var(--bg));
    border-left: 2px solid var(--verdict-green);
  }
  .removed {
    background: color-mix(in srgb, var(--verdict-red) 10%, var(--bg));
    border-left: 2px solid var(--verdict-red);
  }
  @media (max-width: 1100px) {
    .source-items {
      grid-template-columns: 1fr;
    }
    .items {
      border-left: 0;
      border-top: 1px solid var(--line);
    }
  }
</style>

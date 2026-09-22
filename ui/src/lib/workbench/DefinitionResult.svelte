<script lang="ts">
  import { sourceLanguage } from "../sourceLanguage";
  import { V } from "./commands";
  import CodeBlock from "../CodeBlock.svelte";
  import {
    array,
    definition,
    definitionView,
    definitionDiff,
    history,
    object,
    rows,
    string,
    type Json,
  } from "./schema";
  import UpdateResult from "./UpdateResult.svelte";
  import Hash from "./Hash.svelte";
  import DataTable from "./DataTable.svelte";
  import { displayedSource } from "../board/source";
  /** Source shows the rustfmt rendering; this toggle is the only place the stored bytes appear. */
  let asSubmitted = false;
  export let operation: string;
  export let value: Json;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
</script>

{#if [V.update, V.update_view, V.update_repair, V.update_abort, V.update_rebase].some((verb) => verb === operation)}
  <UpdateResult {value} {navigate} />
{/if}
{#if [V.view, V.add, V.update, V.update_repair, V.update_view, V.update_rebase].some((verb) => verb === operation) && typeof object(value, "definition result").hash === "string"}
  {@const def = definitionView(value)}
  {@const lang = sourceLanguage(def.def.lang)}
  {@const shown = displayedSource(
    {
      lang,
      source: def.source,
      formatted_source: def.formatted_source,
      format_error: def.format_error,
    },
    asSubmitted,
  )}
  <div class="section-bar">
    <h2>{def.name ?? def.hash.slice(0, 8)}</h2>
    <span class="muted">{lang}</span>
    <Hash value={def.hash} />
    <span class="muted">{shown.code.split("\n").length} lines</span>
    {#if def.formatted_source !== null}<button
        class="text-control"
        aria-pressed={asSubmitted}
        data-testid="as-submitted"
        on:click={() => (asSubmitted = !asSubmitted)}
        >{asSubmitted ? "formatted" : "as submitted"}</button
      >{/if}
    <button
      class="push text-control"
      on:click={() => navigate(V.run, { target: def.name ?? def.hash })}
      >Run</button
    >
  </div>
  {#if shown.note !== null}<p class="format-note muted" data-testid="format-note">{shown.note}</p>{/if}
  <div class="source-block" data-testid="view-source">
    <CodeBlock code={shown.code} language={lang} />
  </div>
  <details class="extras">
    <summary>Identities, entry effects and {Object.keys(def.items).length} items</summary>
    <div class="identity-strip">
      <span>Definition <Hash value={def.hash} /></span><span
        >Behavior <Hash value={def.behavior_hash} /></span
      >{#if def.wasm_hash}<span>Wasm <Hash value={def.wasm_hash} /></span>{/if}{#if def.toolchain_hash}<span
        >Toolchain <Hash value={def.toolchain_hash} /></span
      >{/if}
    </div>
    <div class="section-bar"><h3>Inferred effects per entry</h3></div>
    <DataTable
      label="Inferred entry effects"
      rows={Object.keys(def.entries).map((name) => ({
        name,
        labels: def.entries[name]!.effects.labels,
        unknown: def.entries[name]!.effects.unknown,
      }))}
    />
    <div class="section-bar"><h3>Items</h3></div>
    <DataTable
      label="Definition items"
      rows={Object.keys(def.items).map((name) => ({
        name,
        hash: def.items[name]!,
      }))}
    />
  </details>
{:else if operation === V.find}
  {@const definitions = array(value, operation).map(definition)}
  <div class="section-bar">
    <h2>Definitions</h2>
    <span class="muted">{definitions.length}</span>
  </div>
  <table aria-label="Definitions">
    <thead><tr><th>Name</th><th>Hash</th><th>Items</th></tr></thead><tbody
      >{#each definitions as def}<tr
          ><td
            ><button
              data-row
              class="text-control"
              on:click={() =>
                navigate(V.view, { target: def.name ?? def.hash })}
              >{def.name ?? "Unnamed"}</button
            ></td
          ><td><Hash value={def.hash} /></td><td
            >{Object.keys(def.items).join(", ")}</td
          ></tr
        >{:else}<tr><td colspan="3" class="empty">No definitions found.</td></tr
        >{/each}</tbody
    >
  </table>
{:else if operation === V.dependents}
  <div class="section-bar"><h2>Dependents</h2></div>
  {#each array(value, operation) as dependent}{@const hash = string(
      dependent,
      "dependent hash",
    )}
    <div class="identity-strip">
      <button
        data-row
        class="text-control"
        on:click={() => navigate(V.view, { target: hash })}
        ><Hash value={hash} /></button
      >
    </div>{:else}<p class="empty">No dependents found.</p>{/each}
{:else if operation === V.history}
  <div class="section-bar"><h2>Hash chain</h2></div>
  {#each history(value) as revision}<section class="revision">
      <div class="section-bar">
        <Hash value={revision.hash} /><time
          class="push muted"
          datetime={new Date(revision.timestamp * 1000).toISOString()}
          >{new Date(revision.timestamp * 1000).toLocaleString()}</time
        ><button
          data-row
          class="text-control"
          disabled={!revision.changes}
          on:click={() =>
            navigate(V.diff, {
              old: revision.changes!.old,
              new: revision.hash,
            })}>Diff parent</button
        >
      </div>
      {#if revision.changes}<DataTable
          label="Changed items"
          rows={revision.changes.changed.map((item) => ({ ...item }))}
        /><DataTable
          label="Added items"
          rows={revision.changes.added.map((item) => ({ ...item }))}
        /><DataTable
          label="Removed items"
          rows={revision.changes.removed.map((item) => ({ ...item }))}
        />{:else}<p class="note">Initial revision.</p>{/if}
    </section>{:else}<p class="empty">No history returned.</p>{/each}
{:else if operation === V.diff}
  {@const diff = definitionDiff(value)}
  <div class="section-bar">
    <h2>Definition diff</h2>
    <Hash value={diff.old} /><span>→</span><Hash value={diff.new} />
  </div>
  <DataTable
    label="Changed items"
    rows={diff.changed.map((item) => ({ ...item }))}
  /><DataTable
    label="Added items"
    rows={diff.added.map((item) => ({ ...item }))}
  /><DataTable
    label="Removed items"
    rows={diff.removed.map((item) => ({ ...item }))}
  />
{:else if operation === V.run}
  {@const result = object(value, "execution result")}
  <div class="section-bar"><h2>Output</h2></div>
  <CodeBlock code={JSON.stringify(result.output, null, 2)} language="json" />
  <div class="section-bar"><h3>Effects performed</h3></div>
  <DataTable
    label="Effects performed"
    rows={rows(result.effects, "execution effects")}
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
  .format-note {
    padding: 4px 12px;
    font-size: 0.92em;
    border-bottom: 1px solid var(--line);
    overflow-wrap: anywhere;
  }
  .source-block :global(.code-block) {
    max-height: none;
  }
  .extras {
    border-top: 1px solid var(--line);
  }
  .extras > summary {
    padding: 8px 12px;
    color: var(--muted);
    cursor: pointer;
  }
  .revision {
    border-bottom: 1px solid var(--line);
  }
</style>

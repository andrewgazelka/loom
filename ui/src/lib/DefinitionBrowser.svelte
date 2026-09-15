<script lang="ts">
  import { sourceLanguage } from "./sourceLanguage";
  import EffectBadges from "./EffectBadges.svelte";
  import SignatureView from "./SignatureView.svelte";
  import RowPreview from "./RowPreview.svelte";
  import {
    AlignLeft,
    ChevronRight,
    Play,
    GitBranch,
    List,
    ArrowLeft,
    Box,
  } from "lucide-svelte";
  import {
    Client,
    resultOf,
    record,
    short,
    bytes,
    type Definition,
    type LogEvent,
  } from "./api";
  import { definitionName, reference } from "./journal";
  import ValueView from "./ValueView.svelte";
  import CodeBlock from "./CodeBlock.svelte";
  import Graph from "./Graph.svelte";
  export let client: Client;
  export let definitions: Definition[] = [];
  export let events: LogEvent[] = [];
  export let inspect: (hash: string) => void;
  export let call: (definition: Definition) => void;
  let query = "";
  let tab = "list",
    selected: Definition | null = null,
    source = "",
    build: unknown = undefined,
    error = "",
    loading = false;
  let edges: { from: string; to: string }[] = [];
  async function graph() {
    tab = "graph";
    try {
      const rows = await Promise.all(
        definitions.map(async (def) => ({
          from: def.hash,
          deps: resultOf(await client.command("deps", { hash: def.hash })),
        })),
      );
      edges = rows.flatMap((row) =>
        Array.isArray(row.deps)
          ? row.deps.map((to) => ({ from: row.from, to: String(to) }))
          : [],
      );
    } catch (e) {
      error = String(e);
    }
  }
  async function preview(def: Definition): Promise<string> {
    const evaluated = events.find(event => reference(record(event.event).def) === def.hash && record(event.event).type === "evaluated");
    if (typeof record(evaluated?.event).source === "string") return String(record(evaluated?.event).source);
    const event = events.find(event => reference(record(record(event.event).def).hash) === def.hash);
    const hash = reference(record(event?.event).source_hash);
    if (!hash) return definitionName(def);
    const data = record(resultOf(await client.command("cas.inspect", {hash})));
    return typeof data.text === "string" ? data.text : definitionName(def);
  }
  async function open(def: Definition) {
    selected = def;
    source = "";
    build = undefined;
    error = "";
    loading = true;
    try {
      const event = events.find(
        (event) => reference(record(record(event.event).def).hash) === def.hash,
      );
      const hash = reference(record(event?.event).source_hash);
      if (hash) source = await client.text(hash);
      if (def.component_hash) {
        const reply = await client.request(
          `builds/${encodeURIComponent(def.component_hash)}`,
        );
        if (reply.ok) {
          build = reply.result;
          const metadata = record(build);
          if (
            typeof metadata.logs_ref === "string" &&
            typeof metadata.logs !== "string"
          )
            build = { ...metadata, logs: await client.text(metadata.logs_ref) };
        }
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }
</script>

<div class="section-heading">
  <div>
    <h1>Definitions</h1>
    <p>Named code. Immutable identities.</p>
  </div>
  <AlignLeft size={18} strokeWidth={1.4} />
</div>
{#if error}<div class="error" role="alert">{error}</div>{/if}
{#if selected}<div class="definition-detail">
    <button class="text-button" on:click={() => (selected = null)}
      ><ArrowLeft size={13} /> All definitions</button
    >
    <div class="definition-title">
      <h2>{definitionName(selected)}</h2>
      <span class="badge">Rust</span
      ><button class="small-button right" on:click={() => call(selected!)}
        ><Play size={12} /> Call</button
      >
    </div>
    <SignatureView value={selected.sig} name={definitionName(selected)} /><EffectBadges inferred={record(selected.sig).effects} allowed={selected.allowed_effects} observed={selected.observed_effects} />
    <button
      class="cid text-button identity"
      on:click={() => inspect(selected!.hash)}>{selected.hash} ↗</button
    >{#if source}<details class="source-card"><summary class="source-summary">Source</summary>
        <CodeBlock
          code={source}
          language={sourceLanguage(selected.lang)}
        />
      </details>{:else if loading}<p class="empty">Reading definition…</p>{/if}
    <details class="plain-details">
      <summary>Signature & identity</summary>
      <div class="detail-body"><ValueView value={selected} {inspect} /></div>
    </details>
    {#if selected.component_hash}<div class="build-heading">
        <Box size={14} />
        <h3>Component build</h3>
        {#if typeof selected.component_size === "number"}<span
            >{bytes(selected.component_size)}</span
          >{/if}
      </div>
      <button
        class="text-button cid identity"
        on:click={() => inspect(selected!.component_hash!)}
        >{selected.component_hash} ↗</button
      >{#if build !== undefined}<div class="build-info">
          {#if typeof record(build).ms === "number"}<span
              >Built in {String(record(build).ms)} ms</span
            >{/if}
        </div>
        {#if typeof record(build).logs === "string" && record(build).logs}<div
            class="source-card"
          >
            <CodeBlock code={String(record(build).logs)} language="text" />
          </div>{:else}<p class="quiet">
            The build completed without log output.
          </p>{/if}
        <details class="plain-details">
          <summary>Build metadata</summary>
          <div class="detail-body"><ValueView value={build} {inspect} /></div>
        </details>{/if}{:else}<p class="quiet build-pending">
        Component builds on first use.
      </p>{/if}
  </div>
{:else}<div class="toolbar">
    <div class="tabs">
      <button aria-pressed={tab === "list"} on:click={() => (tab = "list")}
        >Definitions <span>{definitions.length}</span></button
      ><button aria-pressed={tab === "graph"} on:click={graph}
        >Dependency graph</button
      >
    </div>
    <input class="browser-search" aria-label="Find definitions" placeholder="Find definitions…" bind:value={query} />
  </div>
  {#if tab === "graph"}<Graph {definitions} {edges} />
    <p class="graph-help">
      Two-finger scroll to pan · Pinch to zoom · Arrow keys to pan · 0 to reset
    </p>{:else}<div class="definition-list">
      {#each definitions.filter(def => JSON.stringify(def).toLowerCase().includes(query.toLowerCase())) as def (def.hash)}<div class="definition-item"><button
          class="definition-row"
          on:click={() => open(def)}
          ><AlignLeft size={15} />
          <div>
            <span class="definition-name">{definitionName(def)}</span><SignatureView value={def.sig} name={definitionName(def)} /><EffectBadges inferred={record(def.sig).effects} allowed={def.allowed_effects} observed={def.observed_effects} /><code
              >{short(def.hash, 25)}</code
            >
          </div>
          <span class="badge">{def.lang}</span><span
            class="quiet"
            >{typeof def.component_size === "number"
              ? bytes(def.component_size)
              : def.component_hash
                ? "Built"
                : "On first use"}</span
          ><ChevronRight size={12} /></button
        ><details class="source-peek"><summary>Source</summary><RowPreview code language={sourceLanguage(def.lang)} load={() => preview(def)} /></details></div>{:else}<p class="empty">
          No definitions yet. Save one from the session prompt.
        </p>{/each}
    </div>{/if}{/if}

<style>
  .source-peek { margin:0 10px 12px 39px; color:var(--muted); font-size:10px; }
  .source-peek summary { cursor:pointer; }
  .definition-detail {
    margin-top: 28px;
  }
  .definition-title {
    display: flex;
    align-items: center;
    gap: 12px;
    margin: 22px 0 10px;
  }
  .definition-title h2 {
    font: 13px var(--mono);
    margin: 0;
    overflow-wrap: anywhere;
  }
  .identity {
    font: 10px/1.8 var(--mono) !important;
    overflow-wrap: anywhere;
    text-align: left !important;
    word-break: break-all;
  }
  .source-summary { padding:12px 16px; font-size:11px; cursor:pointer; }
  .source-card {
    margin: 20px 0;
    border: 1px solid var(--line);
    border-radius: 7px;
    overflow: hidden;
  }
  .build-heading {
    display: flex;
    align-items: center;
    gap: 9px;
    margin-top: 30px;
  }
  .build-heading h3 {
    font-size: 12px;
    font-weight: 500;
  }
  .build-heading span {
    margin-left: auto;
    font-size: 10px;
    color: var(--muted);
  }
  .build-info {
    font-size: 10px;
    color: var(--muted);
    margin: 12px 0;
  }
  .build-pending {
    margin: 20px 0;
  }
  .definition-row {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 18px 10px;
    border-bottom: 1px solid var(--line);
    text-align: left;
  }
  .definition-row:hover {
    background: var(--tint);
  }
  .definition-row > div {
    flex: 1;
    min-width: 0;
  }
  .definition-name {
    display: block;
    font: 11px var(--mono);
    overflow-wrap: anywhere;
  }
  .definition-row code {
    display: block;
    font-size: 9px;
    color: var(--muted);
    margin-top: 7px;
  }
  .definition-list {
    border-top: 1px solid var(--line);
  }
  .graph-help {
    font-size: 10px;
    text-align: right;
    color: var(--muted);
    margin-top: 12px;
  }
  @media (max-width: 600px) {
    .definition-row {
      gap: 8px;
    }
  .definition-name {
      font-size: 10px;
    }
    .definition-row > .quiet {
      max-width: 65px;
      font-size: 9px;
    }
    .definition-title {
      flex-wrap: wrap;
    }
  }
</style>

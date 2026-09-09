<script lang="ts">
  import {
    Search,
    ArrowLeft,
    ArrowRight,
    Database,
    Link,
    File,
    ChevronRight,
  } from "lucide-svelte";
  import {
    Client,
    casListing,
    casInspection,
    short,
    bytes,
    type CasEntry,
    type CasInspection,
  } from "./api";
  import ValueView from "./ValueView.svelte";
  import { onMount } from "svelte";
  export let client: Client;
  export let initialHash = "";
  let entries: CasEntry[] = [],
    selected: CasInspection | null = null,
    cursor: string | null = null;
  let kind = "",
    query = "",
    lookup = initialHash,
    busy = false,
    error = "",
    loaded = false,
    history: string[] = [],
    handledInitial = "";
  const kinds = [
    "def",
    "blob",
    "event",
    "desc",
    "result",
    "state",
    "component",
    "source_bundle",
  ];
  async function list(more = false) {
    busy = true;
    error = "";
    try {
      const data = casListing(
        await client.command("cas.list", {
          limit: 50,
          ...(kind ? { kind } : {}),
          ...(query ? { q: query } : {}),
          ...(more && cursor ? { after: cursor } : {}),
        }),
      );
      entries = more ? [...entries, ...data.items] : data.items;
      cursor = data.next_cursor;
      loaded = true;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
  async function inspect(hash: string, remember = true) {
    if (!hash.trim()) return;
    busy = true;
    error = "";
    try {
      const next = casInspection(
        await client.command("cas.inspect", { hash: hash.trim() }),
      );
      if (remember && selected) history = [...history, selected.codec.cid];
      selected = next;
      lookup = next.codec.cid;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
  function back() {
    const prior = history.at(-1);
    history = history.slice(0, -1);
    if (prior) void inspect(prior, false);
    else selected = null;
  }
  onMount(() => {
    void list();
  });
  $: if (initialHash && initialHash !== handledInitial) {
    handledInitial = initialHash;
    lookup = initialHash;
    void inspect(initialHash);
  }
</script>

<div class="section-heading">
  <div>
    <h1>Content store</h1>
    <p>Definitions, events, and values. Every object has an address.</p>
  </div>
  <Database size={18} strokeWidth={1.4} />
</div>
<form class="lookup" on:submit|preventDefault={() => inspect(lookup)}>
  <Search size={14} /><input
    aria-label="Inspect a CID or hash"
    bind:value={lookup}
    placeholder="Inspect a CID or hash"
  /><button class="small-button" disabled={busy || !lookup.trim()}
    >Inspect <ArrowRight size={12} /></button
  >
</form>
{#if error}<div class="error" role="alert">{error}</div>{/if}
{#if selected}<section class="inspection">
    <div class="inspection-top">
      <button class="text-button" on:click={back}
        ><ArrowLeft size={13} />
        {history.length ? "Back" : "All objects"}</button
      ><span>{selected.entry.kind} · {bytes(selected.entry.size)}</span>
    </div>
    <div class="object-heading">
      <File size={19} strokeWidth={1.3} />
      <div>
        <h2>{selected.codec.name}</h2>
        <code class="full-cid">{selected.codec.cid}</code>
      </div>
    </div>
    <dl class="metadata">
      <div>
        <dt>Hash</dt>
        <dd><code>{selected.entry.hash}</code></dd>
      </div>
      <div>
        <dt>Created</dt>
        <dd>
          {new Date(
            selected.entry.created_at *
              (selected.entry.created_at < 1e12 ? 1000 : 1),
          ).toLocaleString()}
        </dd>
      </div>
      <div>
        <dt>Codec</dt>
        <dd>{selected.codec.name} · {selected.codec.code}</dd>
      </div>
    </dl>
    {#if selected.entry.codecs.length > 1}<div class="codec-options">
        <span>Other representations</span
        >{#each selected.entry.codecs as codec}{#if codec.cid !== selected.codec.cid}<button
              class="text-button"
              on:click={() => inspect(codec.cid)}
              >{codec.name}<ArrowRight size={12} /></button
            >{/if}{/each}
      </div>{/if}
    <div class="inspection-content">
      {#if "value" in selected}<div class="eyebrow">DECODED VALUE</div>
        <ValueView
          value={selected.value}
          {inspect}
        />{:else if selected.text !== undefined}<div class="eyebrow">TEXT</div>
        <pre>{selected.text}</pre>{:else}<p class="muted">
          Binary content. View the hexadecimal preview below.
        </p>{/if}
    </div>
    {#if selected.links.length}<div class="object-links">
        <h3><Link size={13} /> Links <span>{selected.links.length}</span></h3>
        {#each selected.links as link}<button on:click={() => inspect(link.cid)}
            ><span class="link-path">{link.path || "/"}</span><code
              >{short(link.cid, 28)}</code
            ><ArrowRight size={12} /></button
          >{/each}
      </div>{/if}
    <details class="raw">
      <summary
        ><ChevronRight size={13} /> Raw bytes
        <span>{selected.truncated ? "Preview truncated" : "Hexadecimal"}</span
        ></summary
      >
      <pre>{selected.hex}</pre>
    </details>
    {#if selected.truncated}<p class="preview-note">
        This is a bounded preview. The complete object is available from the CAS
        API.
      </p>{/if}
  </section>
{:else}<div class="toolbar">
    <label class="filter"
      ><span>Kind</span><select
        aria-label="Filter content by kind"
        bind:value={kind}
        on:change={() => list()}
        ><option value="">All objects</option>{#each kinds as item}<option
            value={item}>{item}</option
          >{/each}</select
      ></label
    >
    <form class="search" on:submit|preventDefault={() => list()}>
      <Search size={13} /><input
        aria-label="Filter content hash prefix"
        bind:value={query}
        placeholder="Hash prefix"
      /><button class="text-button" disabled={busy}>Filter</button>
    </form>
  </div>
  <div class="cas-list">
    <div class="list-heading">
      <span>Object</span><span>Kind</span><span>Size</span>
    </div>
    {#each entries as entry}<button
        class="cas-row"
        on:click={() => inspect(entry.codecs[0]?.cid || entry.hash)}
        ><code title={entry.codecs[0]?.cid || entry.hash}
          >{short(entry.codecs[0]?.cid || entry.hash, 27)}</code
        ><span>{entry.kind}</span><span>{bytes(entry.size)}</span><ChevronRight
          size={12}
        /></button
      >{/each}{#if loaded && !entries.length}<p class="empty">
        No objects match these filters.
      </p>{/if}
  </div>
  {#if cursor}<button
      class="load-more"
      disabled={busy}
      on:click={() => list(true)}
      >{busy ? "Loading…" : "Load more objects"}</button
    >{/if}{/if}
{#if busy}<p class="loading" aria-live="polite">Reading content…</p>{/if}

<style>
  .lookup {
    display: flex;
    align-items: center;
    gap: 10px;
    border: 1px solid var(--line);
    padding: 9px 12px;
    border-radius: 7px;
    background: var(--card);
    margin: 24px 0;
  }
  .lookup input {
    flex: 1;
    min-width: 0;
    border: 0;
    background: transparent;
    font: 11px var(--mono);
    outline: none;
  }
  .inspection {
    border: 1px solid var(--line);
    background: var(--card);
    border-radius: 8px;
    overflow: hidden;
  }
  .inspection-top {
    display: flex;
    justify-content: space-between;
    border-bottom: 1px solid var(--line);
    padding: 12px 18px;
    font-size: 10px;
    color: var(--muted);
  }
  .object-heading {
    display: flex;
    gap: 12px;
    padding: 23px 20px 12px;
  }
  .object-heading h2 {
    font-size: 13px;
    font-weight: 550;
    margin: 0 0 6px;
  }
  .full-cid {
    font-size: 10px;
    overflow-wrap: anywhere;
    color: var(--muted);
  }
  .metadata {
    margin: 0;
    padding: 4px 20px 18px;
    font-size: 11px;
  }
  .metadata > div {
    display: grid;
    grid-template-columns: 80px minmax(0, 1fr);
    padding: 5px 0;
  }
  .metadata dt {
    color: var(--muted);
  }
  .metadata dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .metadata code {
    font-size: 10px;
  }
  .inspection-content {
    padding: 20px;
    border-top: 1px solid var(--line);
    background: var(--code);
  }
  .eyebrow {
    margin-bottom: 14px;
  }
  .inspection pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 11px/1.9 var(--mono);
    margin: 0;
  }
  .object-links {
    border-top: 1px solid var(--line);
    padding: 15px 20px;
  }
  .object-links h3 {
    font-size: 11px;
    display: flex;
    align-items: center;
    gap: 7px;
    font-weight: 500;
  }
  .object-links h3 span {
    color: var(--muted);
  }
  .object-links button {
    display: flex;
    gap: 12px;
    align-items: center;
    width: 100%;
    padding: 8px 0;
    text-align: left;
    font-size: 10px;
  }
  .object-links button code {
    color: var(--link);
    font-size: 10px;
    overflow-wrap: anywhere;
  }
  .link-path {
    min-width: 65px;
    color: var(--muted);
  }
  .raw {
    border-top: 1px solid var(--line);
  }
  .raw summary {
    padding: 13px 20px;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    cursor: pointer;
  }
  .raw summary span {
    margin-left: auto;
    color: var(--muted);
    font-size: 10px;
  }
  .raw pre {
    padding: 20px;
    background: var(--code);
    max-height: 320px;
    overflow: auto;
    word-break: break-all;
  }
  .preview-note {
    padding: 0 20px;
    font-size: 10px;
    color: var(--muted);
  }
  .cas-list {
    border-top: 1px solid var(--line);
  }
  .list-heading,
  .cas-row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 110px 80px 14px;
    gap: 12px;
    align-items: center;
    width: 100%;
    text-align: left;
  }
  .list-heading {
    font-size: 10px;
    color: var(--muted);
    padding: 12px 10px;
  }
  .cas-row {
    padding: 16px 10px;
    border-top: 1px solid var(--line);
    font-size: 11px;
  }
  .cas-row:hover {
    background: var(--tint);
  }
  .cas-row code {
    font-size: 10px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .cas-row > span {
    color: var(--muted);
    font-size: 10px;
  }
  .loading,
  .load-more {
    font-size: 11px;
    color: var(--muted);
    padding: 16px;
  }
  .load-more {
    display: block;
    margin: auto;
  }
  .filter {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--muted);
  }
  .filter select {
    border: 0;
    background: transparent;
    color: var(--ink);
    font-size: 11px;
  }
  .codec-options {
    display: flex;
    gap: 12px;
    padding: 8px 20px 16px;
    font-size: 10px;
    color: var(--muted);
  }
  @media (max-width: 600px) {
    .list-heading,
    .cas-row {
      grid-template-columns: minmax(0, 1fr) 75px 55px 10px;
      gap: 5px;
    }
    .cas-row code {
      font-size: 9px;
    }
    .inspection-top {
      padding: 12px;
    }
    .object-links button {
      flex-wrap: wrap;
    }
    .lookup .small-button {
      font-size: 10px;
      padding: 5px;
    }
  }
</style>

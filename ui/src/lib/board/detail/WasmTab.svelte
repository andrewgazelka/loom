<script lang="ts">
  /**
   * Source on the left, the component's wat on the right, joined by the DWARF line map: hovering
   * or clicking a source line lights every wat line compiled from it; clicking a wat line lights
   * its source line.
   */
  import type { BoardClient } from "../connect";
  import type { CodeLanguage } from "../../highlight";
  import type { DefinitionView } from "../../workbench/schema";
  import SourceView from "../SourceView.svelte";
  import { splitLines } from "../source";
  import WasmView from "../WasmView.svelte";
  import { displayedSource } from "../source";
  import {
    NO_DEBUG_INFO,
    OWN_FILE,
    indexLines,
    sourceOf,
    watLinesFor,
    type WasmModule,
  } from "../wasm";
  import { reason, short } from "./format";
  let {
    lang,
    componentHash,
    exports,
    view,
    viewError,
    client,
  }: {
    lang: string;
    componentHash: string | null;
    exports: string[];
    view: DefinitionView | null;
    viewError: string | null;
    client: BoardClient | null;
  } = $props();

  let module = $state.raw<WasmModule | null>(null);
  let moduleError = $state<string | null>(null);
  let hotSource = $state<number | null>(null);
  let hotWat = $state<number[]>([]);
  /** Which side produced the current mark; the other side scrolls to follow. */
  let origin = $state<"source" | "wasm" | null>(null);
  let foreign = $state<string | null>(null);

  const language = $derived<CodeLanguage>(
    lang === "rust" || lang === "typescript" || lang === "javascript" ? lang : "text",
  );
  // The line table indexes the text the compiler saw: the stored bytes plus the
  // generated entry wrappers. Prefer the server's reconstruction of exactly
  // that; fall back to the stored bytes (never the rustfmt rendering).
  const code = $derived(
    view === null
      ? null
      : module?.compiledSource != null
        ? {
            code: module.compiledSource,
            note: `Source as compiled: stored bytes, then the generated entry wrappers from line ${
              splitLines(view.source).length + 1
            }.`,
            formatted: false,
          }
        : displayedSource(
            {
              lang,
              source: view.source,
              formatted_source: view.formatted_source,
              format_error: view.format_error,
            },
            true,
          ),
  );
  const index = $derived(module === null ? null : indexLines(module.lines));
  const mapped = $derived(module !== null && module.debug && module.lines.length > 0);

  $effect(() => {
    const hash = componentHash;
    const owner = client;
    module = null;
    moduleError = null;
    hotSource = null;
    hotWat = [];
    foreign = null;
    if (hash === null || owner === null) return;
    owner.wasm(hash).then(
      (loaded) => {
        if (componentHash === hash && client === owner) module = loaded;
      },
      (problem: unknown) => {
        if (componentHash === hash && client === owner) moduleError = reason(problem);
      },
    );
  });

  function markSource(line: number | null) {
    if (!index) return;
    origin = "source";
    foreign = null;
    if (line === null) {
      hotSource = null;
      hotWat = [];
      return;
    }
    hotSource = line;
    hotWat = watLinesFor(index, line);
  }
  function markWat(watLine: number) {
    if (!index) return;
    origin = "wasm";
    hotWat = [watLine];
    const source = sourceOf(index, watLine);
    if (source === null) {
      hotSource = null;
      foreign = `wat line ${watLine} has no source mapping`;
    } else if (source.file !== OWN_FILE) {
      hotSource = null;
      foreign = `${source.file}:${source.line} (external)`;
    } else {
      hotSource = source.line;
      foreign = null;
    }
  }
</script>

<div class="wasm-tab" data-testid="detail-wasm" data-debug={module?.debug}>
  {#if componentHash === null}
    <div class="note">No compiled component for this definition.</div>
  {:else if client === null}
    <div class="note">Not connected.</div>
  {:else if moduleError !== null}
    <div class="error" role="alert">{moduleError}</div>
  {:else if module === null}
    <div class="note">Loading wasm {short(componentHash)}…</div>
  {:else}
    <div class="wasm-bar">
      {#if !module.debug}<span class="muted">{NO_DEBUG_INFO}</span
        >{:else if !mapped}<span class="muted"
          >Debug info present, but the line map is empty.</span
        >{:else}<span class="muted"
          >{module.functions.length} functions · {module.lines.length} mapped lines · hover or click a
          source line</span
        >{/if}
      {#if foreign !== null}<span class="foreign">{foreign}</span>{/if}
      {#if code?.note}<span class="muted">{code.note}</span>{/if}
      {#if module.compiledSourceError !== null}<span class="muted"
          >Wrapper text unavailable: {module.compiledSourceError}</span
        >{/if}
    </div>
    <div class="columns">
      <div class="column source-column">
        {#if viewError !== null}<div class="error" role="alert">{viewError}</div
          >{:else if code === null}<div class="note">Loading source…</div
          >{:else}<SourceView
            code={code.code}
            {language}
            testid="wasm-source"
            hot={hotSource}
            follow={origin === "wasm"}
            onhover={mapped ? markSource : undefined}
            onselect={mapped ? markSource : undefined}
          />{/if}
      </div>
      <div class="column wat-column">
        <WasmView
          {module}
          {exports}
          hot={hotWat}
          follow={origin === "source"}
          onselect={mapped ? markWat : undefined}
        />
      </div>
    </div>
  {/if}
</div>

<style>
  .wasm-tab {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  .wasm-bar {
    display: flex;
    gap: 14px;
    align-items: center;
    min-height: 26px;
    padding: 2px 12px;
    border-bottom: 1px solid var(--line);
    font-size: 0.92em;
    flex: none;
  }
  .foreign {
    font-family: var(--mono);
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .columns {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    flex: 1;
    min-height: 0;
  }
  .column {
    overflow: auto;
    min-width: 0;
    min-height: 0;
  }
  .source-column {
    border-right: 1px solid var(--line);
  }
</style>

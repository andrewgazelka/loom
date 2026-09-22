<script lang="ts">
  /**
   * The component's wat text grouped by function. Own functions start expanded, external ones
   * collapse to one header line. Every drawn wat line is `[data-wat-line=N]` and, when the line
   * map covers it, carries `data-src-line` and `data-src-file`.
   */
  import { untrack } from "svelte";
  import { ChevronDown, ChevronRight } from "lucide-svelte";
  import {
    OWN_FILE,
    groupFunctions,
    groupOf,
    indexLines,
    sourceOf,
    type WasmModule,
    type WatGroup,
  } from "./wasm";
  import { splitLines } from "./source";
  let {
    module,
    exports,
    hot = [],
    follow = false,
    onselect,
  }: {
    module: WasmModule;
    exports: string[];
    /** Wat lines marked hot (ascending). */
    hot?: number[];
    follow?: boolean;
    onselect?: (watLine: number) => void;
  } = $props();

  const lines = $derived(splitLines(module.wat));
  const index = $derived(indexLines(module.lines));
  // `exports` arrives through a props object the parent rebuilds per board event; the same
  // array reference passes through this derived unchanged, so `groups` recomputes only when
  // the module or the export list really changed.
  const exportNames = $derived(exports);
  const groups = $derived(
    groupFunctions(module.functions, exportNames, lines.length),
  );
  let expanded = $state<Set<string>>(new Set());
  let root = $state<HTMLElement | null>(null);
  const hotSet = $derived(new Set(hot));
  const width = $derived(String(lines.length).length);

  // A new module resets the fold state to "own functions open"; nothing else does.
  $effect(() => {
    void module;
    expanded = new Set(
      untrack(() => groups)
        .filter((group) => group.own)
        .map((group) => group.key),
    );
  });
  // A hot line inside a collapsed function opens it, then the first hot line scrolls into view.
  $effect(() => {
    if (!follow || !hot.length) return;
    const first = hot[0]!;
    const group = groupOf(groups, first);
    if (group && !expanded.has(group.key)) {
      const next = new Set(expanded);
      next.add(group.key);
      expanded = next;
    }
    queueMicrotask(() => {
      root
        ?.querySelector<HTMLElement>(`[data-wat-line="${first}"]`)
        ?.scrollIntoView({ block: "center" });
    });
  });
  function toggle(group: WatGroup) {
    const next = new Set(expanded);
    if (next.has(group.key)) next.delete(group.key);
    else next.add(group.key);
    expanded = next;
  }
  function range(group: WatGroup): number[] {
    const result: number[] = [];
    for (let line = group.start; line <= group.end; line++) result.push(line);
    return result;
  }
</script>

<div class="wat" bind:this={root} role="presentation">
  {#each groups as group (group.key)}
    {@const open = expanded.has(group.key)}
    <div class="group" class:external={!group.own}>
      <button
        type="button"
        class="head"
        aria-expanded={open}
        data-function={group.fn?.index ?? ""}
        onclick={() => toggle(group)}
        >{#if open}<ChevronDown size={12} />{:else}<ChevronRight
            size={12}
          />{/if}<span class="fn-name" title={group.label}>{group.label}</span
        >{#if group.fn?.exported}<span class="tag">export</span>{/if}<span
          class="muted numeric">{group.end - group.start + 1} lines</span
        ></button
      >
      {#if open}
        {#each range(group) as line (line)}
          {@const source = sourceOf(index, line)}
          <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
          <div
            class="line"
            class:hot={hotSet.has(line)}
            class:own={source?.file === OWN_FILE}
            class:foreign={source !== null && source.file !== OWN_FILE}
            data-wat-line={line}
            data-src-line={source?.line}
            data-src-file={source?.file}
            title={source ? `${source.file}:${source.line}` : undefined}
            onclick={() => onselect?.(line)}
          >
            <span class="gutter" style={`width:${width + 1}ch`}>{line}</span>
            <span class="text">{lines[line - 1] ?? ""}</span>
          </div>
        {/each}
      {/if}
    </div>
  {/each}
</div>

<style>
  .wat {
    font: 0.92em/1.5 var(--mono);
    background: var(--code);
    min-height: 100%;
    padding: 8px 0 24px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    text-align: left;
    padding: 3px 10px;
    font: inherit;
    color: var(--ink);
    position: sticky;
    top: 0;
    background: var(--side);
    border-top: 1px solid var(--line);
    border-bottom: 1px solid var(--line);
  }
  .external .head {
    color: var(--muted);
  }
  .fn-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .tag {
    border: 1px solid var(--line);
    border-radius: 3px;
    padding: 0 4px;
    font-size: 0.85em;
    color: var(--muted);
  }
  .head .numeric {
    margin-left: auto;
    font-size: 0.9em;
  }
  .line {
    display: flex;
    white-space: pre;
    padding: 0 12px 0 8px;
    cursor: pointer;
  }
  .line:hover {
    background: var(--selection);
  }
  .line.hot {
    background: var(--selection);
    box-shadow: inset 2px 0 0 var(--ink);
  }
  .line.own .gutter {
    color: var(--ink);
  }
  .line.foreign .text {
    color: var(--muted);
  }
  .gutter {
    flex: none;
    text-align: right;
    margin-right: 14px;
    color: var(--muted);
    user-select: none;
    font-variant-numeric: tabular-nums;
  }
  .text {
    flex: 1;
    min-width: 0;
  }
</style>

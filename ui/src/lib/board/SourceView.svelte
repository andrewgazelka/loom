<script lang="ts">
  /**
   * Source with a line-number gutter and Shiki tokens. Every line is `[data-line=N]`; `hot`
   * marks one line, `follow` scrolls it into view when the mark came from the other pane.
   */
  import type { CodeLanguage } from "../highlight";
  import { splitLines } from "./source";
  // Shiki (and its WASM engine) loads on first use, not on import: the registry tests import
  // this component without a browser.
  const highlighter = () =>
    import("../highlight").then((module) => module.highlighter);
  let {
    code,
    language,
    hot = null,
    follow = false,
    testid = "detail-source",
    onhover,
    onselect,
  }: {
    code: string;
    language: CodeLanguage;
    hot?: number | null;
    follow?: boolean;
    testid?: string;
    onhover?: (line: number | null) => void;
    onselect?: (line: number) => void;
  } = $props();

  interface Token {
    text: string;
    style: string;
  }
  // Primitive deriveds: the parent may hand these down through an object it rebuilds on every
  // render, and a derived that returns the same string stops that churn here.
  const source = $derived(code);
  const lang = $derived(language);
  const plain = $derived(splitLines(source));
  let tokens = $state.raw<Token[][] | null>(null);
  let highlightError = $state("");
  let root = $state<HTMLElement | null>(null);
  let revision = 0;

  $effect(() => {
    const text = source;
    const grammar = lang;
    const current = ++revision;
    tokens = null;
    highlightError = "";
    if (grammar === "text") return;
    highlighter().then(
      (engine) => {
        if (current !== revision) return;
        try {
          const result = engine.codeToTokens(text, {
            lang: grammar,
            themes: { light: "github-light", dark: "github-dark" },
            defaultColor: false,
          });
          tokens = result.tokens.map((line) =>
            line.map((token) => ({
              text: token.content,
              style: Object.entries(token.htmlStyle ?? {})
                .map(([key, value]) => `${key}:${value}`)
                .join(";"),
            })),
          );
        } catch (problem) {
          highlightError = `Syntax highlighting: ${problem instanceof Error ? problem.message : String(problem)}`;
        }
      },
      (problem: unknown) => {
        if (current === revision)
          highlightError = `Syntax highlighting: ${problem instanceof Error ? problem.message : String(problem)}`;
      },
    );
  });

  $effect(() => {
    if (!follow || hot === null || !root) return;
    root
      .querySelector<HTMLElement>(`[data-line="${hot}"]`)
      ?.scrollIntoView({ block: "center" });
  });

  const width = $derived(String(plain.length).length);
</script>

<div
  class="source"
  data-testid={testid}
  bind:this={root}
  role="presentation"
  onmouseleave={() => onhover?.(null)}
>
  {#if highlightError}<div class="highlight-error" role="alert">
      {highlightError}
    </div>{/if}
  {#each plain as text, index (index)}
    {@const line = index + 1}
    <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
    <div
      class="line"
      class:hot={hot === line}
      class:mapped={onselect !== undefined}
      data-line={line}
      onmouseenter={() => onhover?.(line)}
      onclick={() => onselect?.(line)}
    >
      <span class="gutter" style={`width:${width + 1}ch`}>{line}</span>
      <span class="text"
        >{#if tokens && tokens[index]}{#each tokens[index] as token, position (position)}<span
              class="tok"
              style={token.style}>{token.text}</span
            >{/each}{:else}{text}{/if}</span
      >
    </div>
  {/each}
</div>

<style>
  .source {
    font: 0.95em/1.55 var(--mono);
    background: var(--code);
    min-height: 100%;
    padding: 8px 0 24px;
  }
  .highlight-error {
    padding: 4px 12px;
    color: var(--error);
    font-family: var(--sans);
  }
  .line {
    display: flex;
    white-space: pre;
    padding: 0 12px 0 8px;
  }
  .line.mapped {
    cursor: pointer;
  }
  .line:hover {
    background: var(--selection);
  }
  .line.hot {
    background: var(--selection);
    box-shadow: inset 2px 0 0 var(--ink);
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
    tab-size: 4;
  }
  .tok {
    color: var(--shiki-light);
  }
  @media (prefers-color-scheme: dark) {
    .tok {
      color: var(--shiki-dark);
    }
  }
</style>

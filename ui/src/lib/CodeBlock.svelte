<script lang="ts">
  import { highlighter, type CodeLanguage } from "./highlight";
  export let inline = false;
  let error = "";
  export let code: string;
  export let language: CodeLanguage = "text";
  let html = "",
    revision = 0;
  async function render(source: string, lang: CodeLanguage) {
    const current = ++revision;
    error = "";
    try {
      const engine = await highlighter;
      const result = engine.codeToHtml(source, {
        lang,
        themes: { light: "github-light", dark: "github-dark" },
        defaultColor: false,
      });
      if (current === revision) html = result;
    } catch (problem) {
      if (current === revision) {
        html = "";
        error = `Syntax highlighting: ${String(problem)}`;
      }
    }
  }
  $: void render(code, language);
</script>

<div class="code-block" class:inline>
  {#if error}<span role="alert">{error}</span>{/if}
  {#if html}{@html html}{:else}<pre><code>{code}</code></pre>{/if}
</div>

<style>
  .code-block {
    white-space: normal;
    overflow: auto;
    font: 0.92em/1.65 var(--mono);
    max-height: 500px;
  }
  .code-block :global(pre) {
    white-space: pre;
    font: inherit;
    margin: 0;
    padding: 15px 18px;
    background: var(--code) !important;
    overflow: auto;
  }
  .code-block :global(code) {
    font: inherit;
  }
  .code-block :global(.shiki),
  .code-block :global(.shiki span) {
    color: var(--shiki-light);
  }
  .code-block :global(.shiki span) {
    background: transparent;
  }
  @media (prefers-color-scheme: dark) {
    .code-block :global(.shiki),
    .code-block :global(.shiki span) {
      color: var(--shiki-dark);
    }
  }
  .inline {
    font-size: inherit;
    line-height: inherit;
    max-height: none;
  }
  .inline :global(pre) {
    padding: 0;
    background: transparent !important;
  }
</style>

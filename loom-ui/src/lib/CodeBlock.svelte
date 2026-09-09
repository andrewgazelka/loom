<script lang="ts">
  import { highlighter, type CodeLanguage } from "./highlight";
  export let code: string;
  export let language: CodeLanguage = "text";
  let html = "",
    revision = 0;
  async function render(source: string, lang: CodeLanguage) {
    const current = ++revision;
    try {
      const engine = await highlighter;
      const result = engine.codeToHtml(source, {
        lang,
        themes: { light: "github-light", dark: "github-dark" },
        defaultColor: false,
      });
      if (current === revision) html = result;
    } catch {
      if (current === revision) html = "";
    }
  }
  $: void render(code, language);
</script>

<div class="code-block">
  {#if html}{@html html}{:else}<pre><code>{code}</code></pre>{/if}
</div>

<style>
  .code-block {
    overflow: auto;
    font: 11px/1.9 var(--mono);
    max-height: 500px;
  }
  .code-block :global(pre) {
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
    background-color: var(--shiki-light-bg);
  }
  @media (prefers-color-scheme: dark) {
    .code-block :global(.shiki),
    .code-block :global(.shiki span) {
      color: var(--shiki-dark);
      background-color: var(--shiki-dark-bg);
    }
  }
</style>

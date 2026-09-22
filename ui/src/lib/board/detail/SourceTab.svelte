<script lang="ts">
  /** The definition's source, formatted by default; "as submitted" is the only place the stored bytes appear. */
  import type { DefinitionView } from "../../workbench/schema";
  import type { CodeLanguage } from "../../highlight";
  import SourceView from "../SourceView.svelte";
  import { displayedSource } from "../source";
  let {
    lang,
    view,
    viewError,
  }: { lang: string; view: DefinitionView | null; viewError: string | null } = $props();
  let asSubmitted = $state(false);
  const language = $derived<CodeLanguage>(
    lang === "rust" || lang === "typescript" || lang === "javascript" ? lang : "text",
  );
  const shown = $derived(
    view === null
      ? null
      : displayedSource(
          {
            lang,
            source: view.source,
            formatted_source: view.formatted_source,
            format_error: view.format_error,
          },
          asSubmitted,
        ),
  );
</script>

{#if viewError !== null}
  <div class="error" role="alert">{viewError}</div>
{:else if shown === null}
  <div class="note">Loading source…</div>
{:else}
  <div class="source-bar">
    {#if shown.note !== null}<span class="muted note-line">{shown.note}</span>{/if}
    {#if view && view.formatted_source !== null}
      <button
        type="button"
        class="text-control push"
        aria-pressed={asSubmitted}
        onclick={() => (asSubmitted = !asSubmitted)}
        >{asSubmitted ? "formatted" : "as submitted"}</button
      >
    {/if}
  </div>
  <SourceView code={shown.code} {language} />
{/if}

<style>
  .source-bar {
    display: flex;
    align-items: center;
    gap: 12px;
    min-height: 26px;
    padding: 2px 12px;
    border-bottom: 1px solid var(--line);
    font-size: 0.92em;
    flex: none;
  }
  .note-line {
    overflow-wrap: anywhere;
  }
  .source-bar:empty {
    display: none;
  }
</style>

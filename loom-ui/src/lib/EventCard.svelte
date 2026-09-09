<script lang="ts">
  import {
    AlignLeft,
    Play,
    Circle,
    ArrowUp,
    Check,
    AlertCircle,
    ChevronRight,
    Terminal,
    Clock,
    ExternalLink,
  } from "lucide-svelte";
  import { format, record, short } from "./api";
  import { definitionHash, type JournalRow } from "./journal";
  import SignatureView from "./SignatureView.svelte";
  import ReferenceLink from "./ReferenceLink.svelte";
  import CodeBlock from "./CodeBlock.svelte";
  import type { CodeLanguage } from "./highlight";
  let language: CodeLanguage = "typescript";
  import ValueView from "./ValueView.svelte";
  import EvaluationView from "./EvaluationView.svelte";
  export let row: JournalRow;
  export let inspect: (hash: string) => void;
  export let loadSource: (hash: string) => Promise<string>;
  let source = "",
    sourceError = "",
    loading = false;
  $: definition = row.kind === "defined" || row.kind.startsWith("define");
  $: hash = definitionHash(row);
  $: evaluated = row.kind === "eval" || row.kind === "evaluated";
  $: diagnostic = row.entry?.reply?.diagnostics ?? [];
  $: failed = !!row.entry?.error || row.entry?.reply?.ok === false;
  $: pending = !!row.entry && !row.entry.reply && !row.entry.error;
  $: language = row.language === "rust" ? "rust" : "typescript";
  $: title = definition
    ? row.title
    : evaluated
      ? row.source || row.title
      : row.title;
  $: build = record(record(row.entry?.reply?.result).build);
  async function openSource(event: Event) {
    if (
      (event.currentTarget as HTMLDetailsElement).open &&
      row.sourceRef &&
      !source &&
      !loading
    ) {
      loading = true;
      try {
        source = await loadSource(row.sourceRef);
      } catch (e) {
        sourceError = String(e);
      } finally {
        loading = false;
      }
    }
  }
</script>

<section class="journal-event" aria-label={`${row.kind}: ${title}`}>
  <div class="event-icon" class:failed>
    {#if failed}<AlertCircle size={17} />{:else if definition}<AlignLeft
        size={17}
      />{:else if evaluated}<Play
        size={16}
      />{:else if row.kind.includes("upgrade")}<ArrowUp
        size={17}
      />{:else}<Circle size={14} />{/if}
  </div>
  <div class="event-body">
    {#if definition}<details
        class="code-disclosure"
        open={failed}
        on:toggle={openSource}
      >
        <summary
          ><ChevronRight size={13} /><div class="signature"><span>{title}</span><SignatureView value={row.kind === "defined" ? record(row.value).sig : record(record(row.entry?.reply?.result).def).sig} name={title} /></div><span
            class="language-marker"
            >{row.language === "rust" ? "Rust" : "TS"}</span
          ></summary
        >{#if row.source || source}<CodeBlock
            code={row.source || source}
            {language}
          />{:else if loading}<p class="source-note">
            Reading source…
          </p>{:else if sourceError}<p class="source-note failed">
            {sourceError}
          </p>{:else}<div class="structured">
            <ValueView value={row.value} {inspect} />
          </div>{/if}{#if diagnostic.length}<div class="diagnostics">
            {#each diagnostic as item}<div class="diagnostic">
                <div>
                  <AlertCircle size={13} /><strong>{item.code}</strong><span
                    >{item.file}:{item.line}:{item.col}</span
                  >
                </div>
                <p>{item.message}</p>
                {#if item.snippet}<CodeBlock
                    code={item.snippet}
                    {language}
                  />{/if}{#if item.hint}<p class="hint">{item.hint}</p>{/if}
              </div>{/each}
          </div>{/if}
      </details>
      <div class="event-note">
        {#if pending}<Clock size={11} /><span>Checking and building…</span
          >{/if}{#if hash}<ReferenceLink {hash} {inspect} subtle />{/if}{#if typeof build.ms === "number"}<span
            >{build.ms} ms build</span
          >{/if}{#if row.seq}<span class="right">seq {row.seq}</span>{/if}
      </div>
    {:else if evaluated || row.entry}<div class="execution-card">
        {#if evaluated && row.value !== undefined && !failed}
          <EvaluationView
            value={row.value}
            source={row.source || title}
            {inspect}
          />
        {:else}
          <div class="invocation">
            <Terminal size={13} />
            <div class="invocation-code">
              <CodeBlock
                code={row.source || title}
                language={evaluated ? "typescript" : "json"}
              />
            </div>
          </div>
          {#if row.value !== undefined && !failed}<div class="execution-result">
              <ValueView value={row.value} {inspect} />
            </div>{/if}
        {/if}
        {#if failed}<div class="error-inline">
            {#if row.entry?.error}{row.entry
                .error}{:else}{#each diagnostic as item}<p>
                  <strong>{item.code}</strong>
                  {item.message}
                </p>{/each}{#if !diagnostic.length}{format(row.value)}{/if}{/if}
          </div>{/if}
        <div class="execution-status">
          {#if pending}<Clock size={11} /><span>Running…</span
            >{:else if failed}<AlertCircle size={11} /><span
              >Request rejected</span
            >{:else}<Check size={11} /><span>Completed</span
            >{/if}{#if row.entry?.ms !== undefined}<span>{row.entry.ms} ms</span
            >{/if}{#if row.seq}<span class="right">seq {row.seq}</span>{/if}
        </div>
      </div>
    {:else}<details class="event-disclosure">
        <summary
          ><ChevronRight size={13} /><span>{title}</span><span class="right"
            >seq {row.seq}</span
          ></summary
        >
        <div class="structured">
          <ValueView value={row.metadata} {inspect} />
        </div>
      </details>{/if}
    {#if row.occurrences}<details class="metadata-disclosure">
        <summary><ChevronRight size={11} />{row.occurrences.length} recordings</summary>
        {#each row.occurrences as occurrence (occurrence.id)}
          <details class="metadata-disclosure">
            <summary><ChevronRight size={11} />seq {occurrence.seq}</summary>
            <div class="structured"><ValueView value={occurrence.metadata} {inspect} /></div>
          </details>
        {/each}
      </details>{/if}
    {#if row.metadata && !row.occurrences && (definition || evaluated || row.entry)}<details
        class="metadata-disclosure"
      >
        <summary
          ><ChevronRight size={11} />
          {definition ? "Definition metadata" : "Event details"}</summary
        >
        <div class="structured">
          <ValueView value={row.metadata} {inspect} />
        </div>
      </details>{/if}
  </div>
</section>

<style>
  .journal-event {
    position: relative;
    display: grid;
    grid-template-columns: 28px minmax(0, 1fr);
    gap: 16px;
    padding-bottom: 18px;
  }
  .journal-event:before {
    content: "";
    position: absolute;
    top: 38px;
    bottom: 2px;
    left: 13px;
    border-left: 1px solid var(--line);
  }
  .journal-event:last-child:before {
    display: none;
  }
  .event-icon {
    width: 28px;
    height: 28px;
    margin-top: 10px;
    display: grid;
    place-items: center;
    color: var(--muted);
  }
  .event-body {
    min-width: 0;
  }
  .signature {
    font: 12px/1.85 var(--mono);
    overflow-wrap: anywhere;
  }
  .code-disclosure {
    border: 1px solid transparent;
    border-radius: 8px;
  }
  .code-disclosure[open] {
    border-color: var(--line);
    background: var(--card);
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    list-style: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .code-disclosure > summary {
    padding: 12px 15px;
    min-height: 46px;
    border-radius: 7px;
  }
  .code-disclosure > summary:hover {
    background: var(--code);
  }
  details[open] > summary :global(svg:first-child) {
    transform: rotate(90deg);
  }
  .code-disclosure > :global(.code-block) {
    border-top: 1px solid var(--line);
  }
  .language-marker {
    margin-left: auto;
    font: 9px var(--mono);
    color: var(--muted);
    opacity: 0.7;
  }
  .event-note {
    padding: 7px 16px 0;
    color: var(--muted);
    font-size: 10px;
    display: flex;
    gap: 9px;
    align-items: center;
  }
  .right {
    margin-left: auto;
    white-space: nowrap;
    color: var(--muted);
    font: 9px var(--mono);
  }
  .execution-card {
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--card);
    overflow: hidden;
  }
  .invocation {
    display: flex;
    align-items: center;
    padding: 0 16px;
    gap: 8px;
  }
  .invocation > :global(svg) {
    color: var(--muted);
    flex: none;
  }
  .invocation-code {
    flex: 1;
    min-width: 0;
  }
  .invocation-code :global(pre) {
    background: var(--card) !important;
    padding: 14px 0;
  }
  .invocation-code :global(.shiki span) {
    background: none;
  }
  .execution-result {
    border-top: 1px solid var(--line);
    background: var(--code);
    padding: 15px 18px;
  }
  .execution-result :global(pre) {
    padding: 0;
    font-size: 13px;
  }
  .execution-status {
    border-top: 1px solid var(--line);
    padding: 9px 17px;
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 10px;
    color: var(--muted);
    background: var(--code);
  }
  .structured {
    padding: 14px 18px;
    background: var(--code);
    border-top: 1px solid var(--line);
  }
  .metadata-disclosure {
    margin-top: 5px;
  }
  .metadata-disclosure > summary {
    font-size: 10px;
    color: var(--muted);
    padding: 5px 16px;
  }
  .metadata-disclosure .structured {
    border: 1px solid var(--line);
    border-radius: 6px;
    margin-top: 5px;
  }
  .event-disclosure > summary {
    font-size: 11px;
    padding: 14px 16px;
    color: var(--muted);
  }
  .event-disclosure[open] {
    border: 1px solid var(--line);
    border-radius: 7px;
  }
  .failed,
  .error-inline {
    color: var(--error);
  }
  .error-inline {
    font-size: 11px;
    padding: 14px 18px;
    border-top: 1px solid var(--line);
  }
  .diagnostic {
    color: var(--error);
    border-top: 1px solid var(--line);
    padding: 12px 16px;
    font-size: 11px;
  }
  .diagnostic > div:first-child {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .diagnostic > div > span {
    color: var(--muted);
    margin-left: auto;
    font-size: 10px;
  }
  .diagnostic p {
    margin: 7px 0;
  }
  .hint {
    color: var(--muted);
  }
  .source-note {
    padding: 15px;
    font-size: 11px;
    color: var(--muted);
  }
  @media (max-width: 600px) {
    .journal-event {
      gap: 8px;
      grid-template-columns: 24px minmax(0, 1fr);
    }
    .journal-event:before {
      left: 11px;
    }
    .signature {
      font-size: 10px;
    }
    .code-disclosure > summary {
      padding: 10px;
    }
    .event-note {
      padding-left: 10px;
    }
    .invocation {
      padding: 0 12px;
    }
  }
</style>

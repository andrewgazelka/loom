<script lang="ts">
  import { onMount } from "svelte";
  import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
  import { Compartment, EditorState } from "@codemirror/state";
  import {
    EditorView,
    keymap,
    drawSelection,
    highlightActiveLine,
    lineNumbers,
  } from "@codemirror/view";
  import {
    defaultKeymap,
    history,
    historyKeymap,
    indentWithTab,
  } from "@codemirror/commands";
  import {
    bracketMatching,
    indentOnInput,
    syntaxHighlighting,
    HighlightStyle,
  } from "@codemirror/language";
  import { sql } from "@codemirror/lang-sql";
  import { rust } from "@codemirror/lang-rust";
  import { json } from "@codemirror/lang-json";
  import { tags } from "@lezer/highlight";
  export let value = "";
  export let language: "rust" | "json" | "sql" = "rust";
  export let label = "Source code";
  export let disabled = false;
  export let submit: () => Promise<void>;
  let host: HTMLDivElement;
  let view: EditorView | undefined;
  const editable = new Compartment();
  const grammar = new Compartment();
  const extension = (lang: string) =>
    lang === "rust" ? rust() : lang === "sql" ? sql() : json();
  const colors = HighlightStyle.define([
    { tag: [tags.keyword, tags.operator], color: "var(--syntax-keyword)" },
    { tag: [tags.string, tags.regexp], color: "var(--syntax-string)" },
    { tag: [tags.number, tags.bool, tags.null], color: "var(--syntax-number)" },
    {
      tag: [tags.function(tags.variableName), tags.typeName],
      color: "var(--syntax-function)",
    },
    { tag: tags.comment, color: "var(--muted)", fontStyle: "italic" },
  ]);
  onMount(() => {
    view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        extensions: [
          grammar.of(extension(language)),
          editable.of(EditorState.readOnly.of(disabled)),
          lineNumbers(),
          history(),
          closeBrackets(),
          drawSelection(),
          highlightActiveLine(),
          indentOnInput(),
          bracketMatching(),
          syntaxHighlighting(colors),
          EditorView.lineWrapping,
          EditorView.contentAttributes.of({
            "aria-label": label,
            spellcheck: "false",
          }),
          keymap.of([
            {
              key: "Escape",
              run: () => {
                document
                  .querySelector<HTMLElement>(
                    '[data-pane="explorer"] [data-row]',
                  )
                  ?.focus();
                return true;
              },
            },
            {
              key: "Mod-Enter",
              run: () => {
                if (!disabled) void submit();
                return true;
              },
            },
            indentWithTab,
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...historyKeymap,
          ]),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) value = update.state.doc.toString();
          }),
          EditorView.theme({
            "&": {
              fontSize: "1em",
              color: "var(--ink)",
              backgroundColor: "var(--code)",
            },
            ".cm-gutters": {
              backgroundColor: "var(--side)",
              color: "var(--muted)",
              borderRight: "1px solid var(--line)",
            },
            ".cm-content": {
              fontFamily: "var(--mono)",
              padding: "14px 0",
              minHeight: language === "rust" ? "160px" : "52px",
              caretColor: "var(--ink)",
            },
            ".cm-line": { padding: "0 16px" },
            ".cm-scroller": {
              fontFamily: "var(--mono)",
              lineHeight: "1.6",
              maxHeight: "420px",
              overflow: "auto",
            },
            "&.cm-focused": { outline: "none" },
            ".cm-activeLine": { backgroundColor: "transparent" },
            ".cm-cursor": { borderLeftColor: "var(--ink)" },
            ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": {
              backgroundColor: "var(--selection)",
            },
            ".cm-matchingBracket": {
              backgroundColor: "var(--selection)",
              outline: "1px solid var(--line)",
            },
          }),
        ],
      }),
    });
    return () => view?.destroy();
  });
  $: if (view)
    view.dispatch({
      effects: editable.reconfigure(EditorState.readOnly.of(disabled)),
    });
  $: if (view && view.state.doc.toString() !== value)
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value },
    });
  $: if (view)
    view.dispatch({ effects: grammar.reconfigure(extension(language)) });
</script>

<div class="code-editor" bind:this={host}></div>

<style>
  .code-editor {
    --syntax-keyword: #a626a4;
    --syntax-string: #39713d;
    --syntax-number: #165c96;
    --syntax-function: #7953a6;
    border: 1px solid var(--line);
    border-radius: 6px;
    overflow: hidden;
    --selection: #00000020;
  }
  @media (prefers-color-scheme: dark) {
    .code-editor {
      --syntax-keyword: #ff7b72;
      --syntax-string: #a5d6ff;
      --syntax-number: #79c0ff;
      --syntax-function: #d2a8ff;
      --selection: #ffffff25;
    }
  }
</style>

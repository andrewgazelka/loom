<script lang="ts">
  import { onMount } from "svelte";
  import "@fontsource/jetbrains-mono/400.css";
  import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
  import { EditorState } from "@codemirror/state";
  import { EditorView, keymap, drawSelection, highlightActiveLine, lineNumbers } from "@codemirror/view";
  import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
  import { bracketMatching, indentOnInput, syntaxHighlighting, HighlightStyle } from "@codemirror/language";
  import { setDiagnostics, lintGutter, type Diagnostic } from "@codemirror/lint";
  import { rust } from "@codemirror/lang-rust";
  import { tags as t } from "@lezer/highlight";

  type Problem = { line: number; col: number; message: string; snippet?: string };
  let { value = $bindable(""), problems = [], run }: { value?: string; problems?: Problem[]; run: () => void } = $props();

  let host: HTMLDivElement;
  let view: EditorView | undefined;

  const colors = HighlightStyle.define([
    { tag: [t.keyword, t.controlKeyword, t.definitionKeyword, t.moduleKeyword, t.operatorKeyword], color: "var(--s-keyword)" },
    { tag: [t.typeName, t.className, t.namespace], color: "var(--s-type)" },
    { tag: [t.function(t.variableName), t.function(t.propertyName)], color: "var(--s-function)" },
    { tag: [t.macroName], color: "var(--s-macro)" },
    { tag: [t.string, t.character, t.regexp], color: "var(--s-string)" },
    { tag: [t.number, t.bool, t.null, t.atom], color: "var(--s-number)" },
    { tag: [t.lineComment, t.blockComment, t.docComment], color: "var(--dim)", fontStyle: "italic" },
    { tag: [t.attributeName, t.meta, t.annotation], color: "var(--s-macro)" },
    { tag: [t.labelName, t.self], color: "var(--s-keyword)" },
    { tag: [t.operator, t.punctuation], color: "var(--s-punct)" },
    { tag: t.propertyName, color: "var(--s-prop)" },
  ]);

  onMount(() => {
    view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        extensions: [
          rust(),
          lineNumbers(),
          lintGutter(),
          history(),
          closeBrackets(),
          drawSelection(),
          highlightActiveLine(),
          indentOnInput(),
          bracketMatching(),
          EditorView.lineWrapping,
          syntaxHighlighting(colors),
          EditorView.contentAttributes.of({ spellcheck: "false", "aria-label": "Rust source" }),
          keymap.of([{ key: "Mod-Enter", run: () => (run(), true) }, indentWithTab, ...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) value = u.state.doc.toString();
          }),
          EditorView.theme({
            "&": { height: "auto", backgroundColor: "var(--panel)", color: "var(--ink)", fontSize: "13px", borderRadius: "10px" },
            "&.cm-focused": { outline: "1px solid var(--accent)" },
            ".cm-scroller": { fontFamily: "'JetBrains Mono', ui-monospace, Menlo, monospace", lineHeight: "1.6" },
            ".cm-content": { padding: "12px 0", caretColor: "var(--ink)" },
            ".cm-gutters": { backgroundColor: "transparent", color: "var(--dim)", border: "0" },
            ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "color-mix(in srgb, var(--ink) 6%, transparent)" },
            ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "color-mix(in srgb, var(--accent) 28%, transparent) !important" },
            ".cm-matchingBracket": { backgroundColor: "color-mix(in srgb, var(--accent) 30%, transparent)", outline: "none" },
            ".cm-tooltip": { backgroundColor: "var(--panel)", color: "var(--ink)", border: "1px solid var(--dim)", borderRadius: "6px" },
          }),
        ],
      }),
    });
    return () => view?.destroy();
  });

  // Replace the document when a preset is picked (the editor owns edits, the parent owns presets).
  $effect(() => {
    if (view && value !== view.state.doc.toString()) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: value } });
    }
  });

  // rustc diagnostics become squiggles and gutter markers.
  $effect(() => {
    if (!view) return;
    const doc = view.state.doc;
    const found: Diagnostic[] = problems
      .filter((p) => p.line >= 1 && p.line <= doc.lines && !p.message.startsWith("aborting due to"))
      .map((p) => {
        const line = doc.line(p.line);
        // The server wraps the cell in a function body, so its columns count the wrapper's indent.
        const indent = Math.max((p.snippet?.length ?? line.text.length) - line.text.length, 0);
        const from = Math.min(line.from + Math.max(p.col - 1 - indent, 0), line.to);
        const token = line.text.slice(from - line.from).match(/^("(?:[^"\\]|\\.)*"|[\w:]+|\S)/)?.[0].length ?? 1;
        return { from, to: Math.min(from + token, line.to || from), severity: "error", message: p.message };
      });
    view.dispatch(setDiagnostics(view.state, found));
  });
</script>

<div bind:this={host}></div>

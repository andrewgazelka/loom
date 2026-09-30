<script lang="ts">
  import { onMount } from "svelte";
  import "@fontsource/jetbrains-mono/400.css";
  import "@fontsource/jetbrains-mono/500.css";
  import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
  import { EditorState } from "@codemirror/state";
  import { EditorView, keymap, drawSelection, highlightActiveLine, highlightActiveLineGutter, lineNumbers } from "@codemirror/view";
  import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
  import { bracketMatching, indentOnInput, syntaxHighlighting, HighlightStyle } from "@codemirror/language";
  import { setDiagnostics, lintGutter, type Diagnostic } from "@codemirror/lint";
  import { rust } from "@codemirror/lang-rust";
  import { tags as t } from "@lezer/highlight";

  type Problem = { line: number; col: number; message: string; snippet?: string };
  let { value = $bindable(""), problems = [], run }: { value?: string; problems?: Problem[]; run: () => void } = $props();

  let host: HTMLDivElement;
  let view: EditorView | undefined;

  // Always-dark palette (Tokyo Night family); the page around it follows the OS scheme.
  const c = {
    bg: "#14151c", fg: "#c8d0f0", dim: "#545b7f", keyword: "#bb9af7", type: "#2ac3de", fn: "#7aa2f7",
    macro: "#ff9e64", string: "#9ece6a", number: "#ff9e64", punct: "#7a86b8", prop: "#73daca", accent: "#7aa2f7",
  };

  const colors = HighlightStyle.define([
    { tag: [t.keyword, t.controlKeyword, t.definitionKeyword, t.moduleKeyword, t.operatorKeyword, t.self], color: c.keyword },
    { tag: [t.typeName, t.className, t.namespace], color: c.type },
    { tag: [t.function(t.variableName), t.function(t.propertyName)], color: c.fn },
    { tag: [t.macroName, t.attributeName, t.meta, t.annotation], color: c.macro },
    { tag: [t.string, t.character, t.regexp], color: c.string },
    { tag: [t.number, t.bool, t.null, t.atom], color: c.number },
    { tag: [t.lineComment, t.blockComment, t.docComment], color: c.dim, fontStyle: "italic" },
    { tag: [t.operator, t.punctuation, t.separator, t.derefOperator], color: c.punct },
    { tag: t.propertyName, color: c.prop },
    { tag: t.labelName, color: c.keyword },
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
          highlightActiveLineGutter(),
          indentOnInput(),
          bracketMatching(),
          syntaxHighlighting(colors),
          EditorView.contentAttributes.of({ spellcheck: "false", "aria-label": "Rust source" }),
          keymap.of([{ key: "Mod-Enter", run: () => (run(), true) }, indentWithTab, ...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) value = u.state.doc.toString();
          }),
          EditorView.theme({
            "&": { height: "auto", backgroundColor: c.bg, color: c.fg, fontSize: "13.5px" },
            "&.cm-focused": { outline: "none" },
            ".cm-scroller": {
              fontFamily: "'JetBrains Mono', ui-monospace, Menlo, monospace",
              fontVariantLigatures: "none",
              lineHeight: "1.65",
              overflowY: "hidden",
            },
            ".cm-content": { padding: "14px 0 18px", caretColor: c.accent },
            ".cm-line": { padding: "0 20px 0 6px" },
            ".cm-gutters": { backgroundColor: c.bg, color: c.dim, border: "0", paddingLeft: "6px" },
            ".cm-lineNumbers .cm-gutterElement": { padding: "0 10px 0 8px", minWidth: "26px" },
            ".cm-activeLine": { backgroundColor: "rgba(122,162,247,0.07)" },
            ".cm-activeLineGutter": { backgroundColor: "transparent", color: c.fg },
            ".cm-cursor": { borderLeftColor: c.accent, borderLeftWidth: "2px" },
            ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "rgba(122,162,247,0.28) !important" },
            ".cm-matchingBracket": { backgroundColor: "rgba(122,162,247,0.22)", outline: "none", color: "#fff" },
            ".cm-tooltip": { backgroundColor: "#1d1f2b", color: c.fg, border: "1px solid #2c3048", borderRadius: "8px", padding: "2px 4px" },
            ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy #f7768e", textUnderlineOffset: "3px" },
          }),
        ],
      }),
    });
    return () => view?.destroy();
  });

  // The editor owns typing; the parent owns preset changes.
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

<div bind:this={host} class="editor"></div>

<style>
  .editor { overflow-x: auto; }
  .editor :global(.cm-editor) { min-width: max-content; }
</style>

<script lang="ts">
  import CodeEditor from "./CodeEditor.svelte";
  import Select from "./Select.svelte";
  import { ArrowUp, ChevronRight, Terminal } from "lucide-svelte";
  export let mode = "eval",
    source = "",
    name = "",
    dependencies = "{}",
    busy = false;
  export let submit: () => Promise<void>;
  function changeMode() {
    if (mode === "command") source = '{"command":"actors","args":{}}';
    else if (mode === "define")
      source = "#[loom::def]\nfn main(value: i64) -> i64 {\n    value * 2\n}";
    else source = "";
  }
</script>

<section class="composer" aria-label="Session prompt">
  <div class="composer-toolbar">
    <Terminal size={13} /><Select label="Operation" bind:value={mode} on:change={changeMode} options={[{value:"eval",label:"Evaluate"},{value:"define",label:"Define"},{value:"command",label:"Command"}]} />{#if mode === "define"}<input
        aria-label="Definition name"
        bind:value={name}
        placeholder="Definition name"
      />{:else}<span class="quiet right"
        >{mode === "eval" ? "Rust" : "JSON command"}</span
      >{/if}
  </div>
  <CodeEditor bind:value={source} language={mode === "command" ? "json" : "rust"} {submit} />{#if mode === "define"}<details class="dependency-options">
      <summary><ChevronRight size={11} /> Dependencies</summary><label
        >Import names → definition hashes<input
          aria-label="Dependency names and hashes"
          bind:value={dependencies}
          spellcheck="false"
          placeholder={'{"worker":"#hash"}'}
        /></label
      >
    </details>{/if}
  <div class="composer-footer">
    <span>⌘ Enter to {mode === "define" ? "define" : "run"}</span><button
      class="primary"
      on:click={submit}
      disabled={busy || !source.trim() || (mode === "define" && !name.trim())}
      >{busy
        ? "Running…"
        : mode === "define"
          ? "Save definition"
          : "Run"}<ArrowUp size={13} /></button
    >
  </div>
</section>

<style>
  .composer {
    border: 1px solid var(--line);
    border-radius: 9px;
    background: var(--card);
    overflow: hidden;
    margin: 24px 0 12px;
  }
  .composer-toolbar {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--line);
    color: var(--muted);
  }
  .composer-toolbar :global(.select) {
    border: 0;
    background: transparent;
    color: var(--ink);
    font-size: 11px;
    max-width: 130px;
  }
  .composer-toolbar input {
    flex: 1;
    min-width: 0;
    border: 0;
    background: transparent;
    color: var(--ink);
    font: 11px var(--mono);
  }

  .composer-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-top: 1px solid var(--line);
    padding: 10px 13px;
  }
  .composer-footer > span {
    color: var(--muted);
    font-size: 10px;
  }
  .dependency-options {
    border-top: 1px solid var(--line);
    font-size: 10px;
  }
  .dependency-options summary {
    padding: 10px 16px;
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    cursor: pointer;
  }
  .dependency-options label {
    display: block;
    padding: 0 16px 12px;
    color: var(--muted);
  }
  .dependency-options input {
    display: block;
    width: 100%;
    font: 10px var(--mono);
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--code);
    color: var(--ink);
    padding: 8px;
    margin-top: 7px;
  }
  .composer-toolbar .quiet {
    font-size: 10px;
  }
  .composer-footer .primary {
    padding: 7px 10px;
    gap: 15px;
  }
</style>

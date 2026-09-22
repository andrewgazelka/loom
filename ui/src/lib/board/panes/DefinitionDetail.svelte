<script lang="ts">
  /**
   * One definition in focus. One header line (name, language, hash, effects, dependencies,
   * dependents, Run, Open in workspace), then the registry tabs with the source first.
   */
  import { ExternalLink, Play } from "lucide-svelte";
  import type { Build, Definition } from "../feed";
  import { definitionView, type DefinitionView } from "../../workbench/schema";
  import DetailTabs from "../detail/DetailTabs.svelte";
  import { reason, short } from "../detail/format";
  import type { PaneShared } from "./types";
  let {
    hash,
    def,
    deps,
    dependents,
    names,
    build,
    onselect,
    client,
  }: {
    hash: string;
    /** `null` when the journal has no `defined` event for the hash. */
    def: Definition | null;
    deps: { alias: string; hash: string; name: string | null }[];
    dependents: Definition[];
    names: Record<string, string | null>;
    build: Build | null;
  } & PaneShared = $props();

  let view = $state.raw<DefinitionView | null>(null);
  let viewError = $state<string | null>(null);
  let runOpen = $state(false);
  let runArgs = $state("[]");
  let runBusy = $state(false);
  let runResult = $state<string | null>(null);
  let runError = $state<string | null>(null);
  let runEffects = $state<number | null>(null);

  $effect(() => {
    const owner = client;
    const target = hash;
    view = null;
    viewError = null;
    runOpen = false;
    runResult = null;
    runError = null;
    runEffects = null;
    if (owner === null) {
      viewError = "Not connected.";
      return;
    }
    owner.command("view", { target }).then(
      (result) => {
        if (client !== owner || hash !== target) return;
        try {
          view = definitionView(result);
        } catch (problem) {
          viewError = `view ${short(target)}: ${reason(problem)}`;
        }
      },
      (problem: unknown) => {
        if (client === owner && hash === target) viewError = reason(problem);
      },
    );
  });

  async function run() {
    const owner = client;
    if (owner === null || runBusy) return;
    let args: unknown;
    try {
      args = JSON.parse(runArgs);
    } catch (problem) {
      runError = `arguments: ${reason(problem)}`;
      return;
    }
    if (!Array.isArray(args)) {
      runError = "arguments: expected a JSON array";
      return;
    }
    runBusy = true;
    runError = null;
    runResult = null;
    runEffects = null;
    try {
      const result = await owner.command("run", { target: hash, args });
      if (client !== owner) return;
      const body =
        typeof result === "object" && result !== null && !Array.isArray(result)
          ? (result as Record<string, unknown>)
          : {};
      runResult = JSON.stringify(body.output ?? result, null, 2);
      runEffects = Array.isArray(body.effects) ? body.effects.length : null;
    } catch (problem) {
      if (client === owner) runError = reason(problem);
    } finally {
      if (client === owner) runBusy = false;
    }
  }
  const workspaceHref = $derived(
    `/workspace/?panel=view&target=${encodeURIComponent(hash)}`,
  );
</script>

{#if def === null}
  <div class="note">
    Definition {short(hash)} is not in the journal this board has read. It may predate the
    snapshot or the hash may be wrong.
    <a class="header-link" href={workspaceHref}>open in workspace</a>
  </div>
{:else}
  <header class="head" data-testid="detail-header">
    <h1 class="name">{def.name ?? short(def.hash)}</h1>
    <span class="muted">{def.lang}</span>
    <span class="pill mono" title={def.hash}>{short(def.hash)}</span>
    {#each def.effects as label (label)}<span class="pill effect" class:call={label === "call"}
        >{label}</span
      >{/each}
    {#if deps.length}
      <span class="muted">deps</span>
      {#each deps as dep (dep.alias)}
        <button
          type="button"
          class="text-control dep"
          data-testid="detail-dep"
          data-hash={dep.hash}
          title={`${dep.alias} → ${dep.hash}`}
          onclick={() => onselect({ kind: "def", hash: dep.hash })}
          >{dep.name ?? dep.alias}</button
        >
      {/each}
    {/if}
    <span
      class="muted"
      title={dependents.map((item) => item.name ?? short(item.hash)).join(", ") || "none"}
      >{dependents.length} dependent{dependents.length === 1 ? "" : "s"}</span
    >
    {#if build?.ms !== null && build?.ms !== undefined}<span class="muted numeric"
        >built in {build.ms} ms</span
      >{/if}
    <span class="push"></span>
    <button
      type="button"
      class="action"
      aria-pressed={runOpen}
      data-testid="detail-run"
      onclick={() => (runOpen = !runOpen)}><Play size={12} /> Run</button
    >
    <a class="action" href={workspaceHref} data-testid="detail-workspace"
      ><ExternalLink size={12} /> Open in workspace</a
    >
  </header>
  {#if runOpen}
    <form
      class="run-box"
      onsubmit={(event) => {
        event.preventDefault();
        void run();
      }}
    >
      <label class="muted" for="detail-run-args">arguments</label>
      <input
        id="detail-run-args"
        class="mono"
        bind:value={runArgs}
        spellcheck="false"
        placeholder="[]"
        aria-label="Run arguments, a JSON array"
        disabled={runBusy}
      />
      <button type="submit" class="action" disabled={runBusy || client === null}
        >{runBusy ? "Running…" : "Enter to run"}</button
      >
      {#if runError !== null}<span class="error-text" role="alert">{runError}</span>{/if}
      {#if runResult !== null}<pre class="run-result" data-testid="detail-run-result">{runResult}</pre
        >{#if runEffects !== null}<span class="muted"
            >{runEffects} effect{runEffects === 1 ? "" : "s"}</span
          >{/if}{/if}
    </form>
  {/if}
  <DetailTabs
    selection={{ kind: "def", hash: def.hash }}
    context={{ kind: "def", def, view, viewError, client, onselect, names }}
  />
{/if}

<style>
  .head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 10px;
    min-height: 40px;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
    flex: none;
  }
  .name {
    font-size: 1.15em;
    font-weight: 600;
  }
  .pill {
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 0 5px;
    font-size: 0.86em;
    color: var(--muted);
    background: var(--side);
  }
  .mono {
    font-family: var(--mono);
  }
  .effect.call {
    color: var(--verdict-red);
    border-color: var(--verdict-red);
  }
  .dep {
    font-weight: 500;
  }
  .action {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: 1px solid var(--line);
    border-radius: 5px;
    padding: 3px 8px;
    color: var(--ink);
    text-decoration: none;
    background: var(--card);
  }
  .action:hover {
    background: var(--selection);
  }
  .action[aria-pressed="true"] {
    background: var(--selection);
  }
  .run-box {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 10px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--line);
    background: var(--side);
    flex: none;
  }
  .run-box input {
    flex: 1;
    min-width: 200px;
    padding: 4px 8px;
  }
  .run-result {
    flex-basis: 100%;
    margin: 0;
    padding: 8px 10px;
    background: var(--code);
    border: 1px solid var(--line);
    border-radius: 5px;
    max-height: 200px;
    overflow: auto;
    font-size: 0.92em;
  }
  .error-text {
    color: var(--error);
  }
</style>

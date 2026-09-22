<script lang="ts">
  import { onMount } from "svelte";
  import { Box } from "lucide-svelte";
  import "@fontsource/inter/latin-400.css";
  import "@fontsource/inter/latin-500.css";
  import "@fontsource/inter/latin-600.css";
  import "@fontsource/jetbrains-mono/latin-400.css";
  import "$lib/app.css";
  import { resolveConnection } from "$lib/workbench/connection";
  import { BoardClient, type ConnectionState } from "$lib/board/connect";
  import {
    apply,
    applyTree,
    empty,
    nextExpiry,
    prune,
    setStages,
    type Model,
  } from "$lib/board/feed";
  import { graphOf, layout } from "$lib/board/layout";
  import { parseStages } from "$lib/board/stages";
  import DefinitionsPane from "$lib/board/DefinitionsPane.svelte";
  import GraphPane from "$lib/board/GraphPane.svelte";
  import FeedPane from "$lib/board/FeedPane.svelte";
  import ActorsPane from "$lib/board/ActorsPane.svelte";
  import BuildsPane from "$lib/board/BuildsPane.svelte";

  // One model; every pane derives from it. `$state.raw` because the reducer returns new objects.
  let model = $state.raw<Model>(empty());
  let connection = $state.raw<ConnectionState>({ kind: "idle" });
  let error = $state("");
  let typeScale = $state(12);
  let help = $state(false);
  let selected = $state("");
  let client: BoardClient | undefined;
  let expiryTimer: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let polling = false;
  const requestedLogs = new Set<string>();

  const graph = $derived.by(() => {
    const { nodes, edges } = graphOf(model);
    return layout(nodes, edges);
  });
  const status = $derived(
    connection.kind === "reconnecting"
      ? `reconnecting in ${Math.round(connection.delayMs / 1000)}s`
      : connection.kind,
  );

  function update(next: Model) {
    model = next;
    armExpiry();
  }
  /** Highlights leave the model by time: one timer for the earliest expiry. */
  function armExpiry() {
    if (expiryTimer !== undefined) clearTimeout(expiryTimer);
    expiryTimer = undefined;
    const at = nextExpiry(model);
    if (at === null) return;
    expiryTimer = setTimeout(
      () => {
        expiryTimer = undefined;
        update(prune(model, Date.now()));
      },
      Math.max(0, at - Date.now()) + 5,
    );
  }
  function fail(message: string) {
    error = message;
  }

  // Build logs: one CAS read per component hash, its outcome recorded on the build.
  $effect(() => {
    // Read the model first so the effect tracks it even before the client exists.
    const builds = Object.values(model.builds);
    const owner = client;
    if (!owner) return;
    for (const build of builds) {
      if (
        build.logsRef === null ||
        build.stages !== null ||
        build.stagesError !== null ||
        requestedLogs.has(build.componentHash)
      )
        continue;
      requestedLogs.add(build.componentHash);
      owner.text(build.logsRef).then(
        (log) => {
          if (client === owner)
            update(setStages(model, build.componentHash, parseStages(log)));
        },
        (problem: unknown) => {
          if (client === owner)
            update(
              setStages(
                model,
                build.componentHash,
                problem instanceof Error ? problem : new Error(String(problem)),
              ),
            );
        },
      );
    }
  });

  async function pollTree() {
    const owner = client;
    if (!owner || polling || document.visibilityState !== "visible") return;
    polling = true;
    try {
      const tree = await owner.command("tree", {});
      if (client === owner) update(applyTree(model, tree, Date.now()));
    } catch (problem) {
      if (client === owner)
        fail(`tree: ${problem instanceof Error ? problem.message : String(problem)}`);
    } finally {
      polling = false;
    }
  }
  function startPolling() {
    stopPolling();
    pollTimer = setInterval(() => void pollTree(), 1000);
    void pollTree();
  }
  function stopPolling() {
    if (pollTimer !== undefined) clearInterval(pollTimer);
    pollTimer = undefined;
  }
  function visibility() {
    if (document.visibilityState === "visible") startPolling();
    else stopPolling();
  }

  function keyboard(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const target = event.target as HTMLElement | null;
    const typing = !!target?.closest("input,textarea,[contenteditable=true]");
    if (event.key === "Escape") {
      help = false;
      if (typing) target?.blur();
      return;
    }
    if (typing) return;
    if (event.key === "?") {
      event.preventDefault();
      help = !help;
    } else if (event.key === "/") {
      event.preventDefault();
      document.getElementById("board-filter")?.focus();
    } else if (event.key === "+" || event.key === "=" || event.key === "-") {
      event.preventDefault();
      typeScale = Math.max(
        10,
        Math.min(18, typeScale + (event.key === "-" ? -1 : 1)),
      );
    } else if (event.key === "j" || event.key === "k") {
      const pane = target?.closest("[data-pane]") ?? document;
      const rows = [...pane.querySelectorAll<HTMLElement>("[data-row]")];
      if (!rows.length) return;
      event.preventDefault();
      const index = rows.indexOf(target as HTMLElement);
      const next =
        rows[
          index < 0
            ? 0
            : Math.max(0, Math.min(rows.length - 1, index + (event.key === "j" ? 1 : -1)))
        ];
      next?.focus();
      next?.scrollIntoView({ block: "nearest" });
    }
  }

  onMount(() => {
    let endpoint = "";
    let token = "";
    try {
      const resolved = resolveConnection({
        location,
        history,
        storage: localStorage,
      });
      if (!resolved) {
        error =
          "No connection. Open /board#token=<token>, or connect once from the workspace page.";
        return;
      }
      endpoint = resolved.endpoint;
      token = resolved.token;
    } catch (problem) {
      error = problem instanceof Error ? problem.message : String(problem);
      return;
    }
    const owner = new BoardClient({
      endpoint,
      token,
      onEvents: (events) => update(apply(model, events, Date.now())),
      onState: (state) => (connection = state),
      onError: fail,
    });
    client = owner;
    owner.start();
    document.addEventListener("visibilitychange", visibility);
    startPolling();
    return () => {
      document.removeEventListener("visibilitychange", visibility);
      stopPolling();
      if (expiryTimer !== undefined) clearTimeout(expiryTimer);
      client = undefined;
      owner.stop();
    };
  });
</script>

<svelte:window onkeydown={keyboard} />
<svelte:head>
  <title>loom · board</title>
  <meta name="color-scheme" content="light dark" />
</svelte:head>

<div class="app-shell" style={`--type-scale:${typeScale}px`}>
  <header class="app-header">
    <Box size={20} class="icon-brand" /><strong class="wordmark">loom</strong
    ><span class="header-path">/</span><span>board</span>
    <a class="header-link" href="/">workspace</a>
    <span
      class="status connection"
      class:live={connection.kind === "live"}
      data-testid="board-status"
      aria-live="polite">{status}</span
    >
    <span class="muted numeric">seq {model.seq}</span>
    <button
      class="push"
      type="button"
      aria-label="Key hints"
      title="Key hints (?)"
      aria-pressed={help}
      onclick={() => (help = !help)}>?</button
    >
  </header>
  {#if help}<div class="help-strip">
      <span><kbd>j / k</kbd> Move through rows in the focused pane</span>
      <span><kbd>/</kbd> Filter the feed</span>
      <span><kbd>drag · wheel · 0</kbd> Pan, zoom, fit the graph</span>
      <span><kbd>+ / −</kbd> Type scale</span>
      <span><kbd>?</kbd> Hide help</span>
    </div>{/if}
  {#if error}<div class="error" role="alert" data-testid="board-error">
      {error}<button class="text-control" type="button" onclick={() => (error = "")}
        >Dismiss</button
      >
    </div>{/if}
  <div class="board">
    <section class="pane left" aria-label="Definitions and builds">
      <div class="stack">
        <DefinitionsPane {model} {selected} onselect={(hash) => (selected = hash)} />
      </div>
      <div class="stack builds">
        <BuildsPane {model} />
      </div>
    </section>
    <section class="pane" aria-label="Graph">
      <GraphPane {model} {graph} {selected} onselect={(hash) => (selected = hash)} />
    </section>
    <section class="pane left" aria-label="Feed">
      <FeedPane {model} />
    </section>
    <section class="pane" aria-label="Actors">
      <ActorsPane {model} />
    </section>
  </div>
  <footer class="status-bar">
    <span class="status-light" class:live={connection.kind === "live"}></span>
    <span>{status}</span>
    <span>{Object.keys(model.definitions).length} definitions</span>
    <span>{Object.keys(model.builds).length} builds</span>
    <span>{model.feed.length} events held</span>
    <span>{model.actorOrder.length} actors</span>
    <span class="push">{typeScale}px</span>
  </footer>
</div>

<style>
  .board {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: minmax(280px, 1fr) minmax(0, 2.4fr);
    grid-template-rows: minmax(0, 3fr) minmax(0, 2fr);
  }
  .pane {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
    border-bottom: 1px solid var(--line);
    background: var(--bg);
  }
  .pane.left {
    border-right: 1px solid var(--line);
  }
  .stack {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
  }
  .stack.builds {
    border-top: 1px solid var(--line);
  }
  .connection {
    font-family: var(--mono);
  }
  .connection.live {
    color: var(--ink);
    border-color: var(--focus);
  }
  .status-light.live {
    background: var(--verdict-green);
  }
  @media (max-width: 800px) {
    .board {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: repeat(4, minmax(0, 1fr));
    }
    .pane.left {
      border-right: 0;
    }
  }
</style>

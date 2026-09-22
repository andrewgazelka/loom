<script lang="ts">
  /**
   * The board: one model fed by the journal and the tree poll, drawn by the panes the registry
   * declares. The Overview shows the rail and the main grid; a selection (`#def=`, `#actor=`,
   * `#build=`, `#run=`) swaps the main grid for that kind's Detail pane.
   */
  import { onMount } from "svelte";
  import { ArrowLeft, Box } from "lucide-svelte";
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
  import { parseStages } from "$lib/board/stages";
  import {
    formatSelection,
    parseSelection,
    sameSelection,
    type Selection,
  } from "$lib/board/selection";
  import { panes } from "$lib/board/panes";
  import {
    compose,
    defaultArrangement,
    loadArrangement,
    saveArrangement,
    toggleHidden,
    toggleablePanes,
    type Arrangement,
  } from "$lib/board/panes/arrangement";
  import PanesMenu from "$lib/board/panes/PanesMenu.svelte";

  // One model; every pane derives from it through its `select`. `$state.raw` because the reducer returns new objects.
  let model = $state.raw<Model>(empty());
  let connection = $state.raw<ConnectionState>({ kind: "idle" });
  let client = $state.raw<BoardClient | null>(null);
  let selection = $state.raw<Selection | null>(null);
  let arrangement = $state.raw<Arrangement>(defaultArrangement(panes));
  let error = $state("");
  let typeScale = $state(12);
  let help = $state(false);
  let expiryTimer: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let polling = false;
  const requestedLogs = new Set<string>();

  const composed = $derived(compose(panes, arrangement, selection));
  const detailPane = $derived(selection === null ? null : (composed.main[0]?.pane ?? null));
  const detailHash = $derived(
    selection !== null && (selection.kind === "def" || selection.kind === "build")
      ? selection.hash
      : null,
  );
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
  const reason = (problem: unknown) =>
    problem instanceof Error ? problem.message : String(problem);

  // Build logs: one CAS read per component hash (the client caches the text for the Log tab),
  // its stage breakdown recorded on the build.
  $effect(() => {
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
      if (client === owner) fail(`tree: ${reason(problem)}`);
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

  // Selection <-> URL fragment. `push` records a history entry so the browser's back returns.
  function applySelection(next: Selection | null, push: boolean) {
    if (sameSelection(next, selection)) return;
    selection = next;
    const url = location.pathname + location.search + formatSelection(next);
    if (push) history.pushState(null, "", url);
    else history.replaceState(null, "", url);
  }
  function select(next: Selection | null) {
    applySelection(next, true);
  }
  function readFragment() {
    try {
      applySelection(parseSelection(location.hash), false);
    } catch (problem) {
      fail(reason(problem));
    }
  }
  function togglePane(id: string) {
    arrangement = toggleHidden(arrangement, id);
    try {
      saveArrangement(localStorage, arrangement);
    } catch (problem) {
      fail(`layout: ${reason(problem)}`);
    }
  }

  function keyboard(event: KeyboardEvent) {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const target = event.target as HTMLElement | null;
    const typing = !!target?.closest("input,textarea,[contenteditable=true]");
    if (event.key === "Escape") {
      if (help) {
        help = false;
        return;
      }
      if (typing) {
        target?.blur();
        return;
      }
      if (selection !== null) select(null);
      return;
    }
    if (typing) return;
    if (event.key === "?") {
      event.preventDefault();
      help = !help;
    } else if (event.key === "/") {
      const filter = document.getElementById("board-filter");
      if (filter) {
        event.preventDefault();
        filter.focus();
      }
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
    try {
      arrangement = loadArrangement(localStorage, panes);
    } catch (problem) {
      fail(`${reason(problem)}; using the default layout.`);
    }
    // The selection is read before the connection store strips a `#token=` fragment.
    let initial: Selection | null = null;
    try {
      initial = parseSelection(location.hash);
    } catch (problem) {
      fail(reason(problem));
    }
    let endpoint = "";
    let token = "";
    try {
      const resolved = resolveConnection({ location, history, storage: localStorage });
      if (!resolved) {
        error =
          "No connection. Open /#token=<token>, or connect once from the command workspace.";
        return;
      }
      endpoint = resolved.endpoint;
      token = resolved.token;
    } catch (problem) {
      error = reason(problem);
      return;
    }
    applySelection(initial, false);
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
    window.addEventListener("popstate", readFragment);
    startPolling();
    return () => {
      document.removeEventListener("visibilitychange", visibility);
      window.removeEventListener("popstate", readFragment);
      stopPolling();
      if (expiryTimer !== undefined) clearTimeout(expiryTimer);
      client = null;
      owner.stop();
    };
  });
</script>

<svelte:window onkeydown={keyboard} />
<svelte:head>
  <title>loom{detailPane ? ` · ${detailPane.title.toLowerCase()}` : ""}</title>
  <meta name="color-scheme" content="light dark" />
</svelte:head>

<div class="app-shell" style={`--type-scale:${typeScale}px`}>
  <header class="app-header">
    <Box size={20} class="icon-brand" /><strong class="wordmark">loom</strong>
    {#if detailPane}<span class="header-path">/</span><span>{detailPane.title.toLowerCase()}</span
      >{/if}
    <a class="header-link" href="/workspace/" data-testid="workspace-link">commands</a>
    <span
      class="status connection"
      class:live={connection.kind === "live"}
      data-testid="board-status"
      aria-live="polite">{status}</span
    >
    <span class="push"></span>
    <PanesMenu options={toggleablePanes(panes)} hidden={arrangement.hidden} ontoggle={togglePane} />
    <button
      type="button"
      aria-label="Key hints"
      title="Key hints (?)"
      aria-pressed={help}
      onclick={() => (help = !help)}>?</button
    >
  </header>
  {#if help}<div class="help-strip">
      <span><kbd>click</kbd> Open a definition, actor, build or run</span>
      <span><kbd>Esc</kbd> Back to the overview</span>
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
  <div class="board" class:detail={selection !== null}>
    <aside class="rail" aria-label="Definitions and actors">
      {#each composed.rail as pane (pane.id)}
        <section class="pane" aria-label={pane.title}>
          <pane.component {...pane.select(model, selection)} onselect={select} {client} />
        </section>
      {:else}
        <div class="empty">Every rail pane is hidden.</div>
      {/each}
    </aside>
    <section class="main" aria-label={selection === null ? "Overview" : "Detail"}>
      {#if selection !== null}
        <div class="detail-bar">
          <button
            type="button"
            class="back"
            data-testid="detail-back"
            onclick={() => select(null)}><ArrowLeft size={13} /> Overview</button
          >
          <span class="muted">Esc</span>
        </div>
      {/if}
      {#key formatSelection(selection)}
        <div
          class="cells"
          data-testid={selection === null ? "board-overview" : "board-detail"}
          data-kind={selection?.kind}
          data-hash={detailHash}
        >
          {#each composed.main as cell (cell.pane.id)}
            <section class="pane cell" class:full={cell.full} aria-label={cell.pane.title}>
              <cell.pane.component
                {...cell.pane.select(model, selection)}
                onselect={select}
                {client}
              />
            </section>
          {:else}
            <div class="empty">Every pane is hidden. Use the panes menu to show one.</div>
          {/each}
        </div>
      {/key}
    </section>
  </div>
  <footer class="status-bar">
    <span class="status-light" class:live={connection.kind === "live"}></span>
    <span>{status}</span>
    <span class="muted numeric">seq {model.seq}</span>
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
    grid-template-columns: 260px minmax(0, 1fr);
  }
  .rail {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-right: 1px solid var(--line);
  }
  .rail .pane {
    flex: 1;
    border-bottom: 1px solid var(--line);
  }
  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }
  .detail-bar {
    display: flex;
    align-items: center;
    gap: 10px;
    min-height: 30px;
    padding: 0 8px;
    border-bottom: 1px solid var(--line);
    background: var(--side);
    flex: none;
    font-size: 0.92em;
  }
  .back {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 7px;
    border-radius: 4px;
  }
  .cells {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    grid-auto-rows: minmax(0, 1fr);
    gap: 1px;
    background: var(--line);
  }
  .board.detail .cells {
    grid-template-columns: minmax(0, 1fr);
  }
  .cell.full {
    grid-column: 1 / -1;
  }
  .pane {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
    background: var(--bg);
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
  .cells > .empty {
    grid-column: 1 / -1;
    background: var(--bg);
  }
  @media (max-width: 800px) {
    .board {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: minmax(0, 2fr) minmax(0, 3fr);
    }
    .rail {
      border-right: 0;
      flex-direction: row;
    }
    .rail .pane {
      border-bottom: 0;
      border-right: 1px solid var(--line);
    }
    .cells {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>

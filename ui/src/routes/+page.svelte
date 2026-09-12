<script lang="ts">
  import { onMount, onDestroy, tick } from "svelte";
  import {
    Box,
    FileCode2,
    Network,
    GitBranch,
    Search,
    Settings2,
    RefreshCw,
    ChevronRight,
    ChevronDown,
    Circle,
    Plus,
  } from "lucide-svelte";
  import { fragmentToken } from "$lib/workbench/fragment-token";
  const connectionKey = "loom.connection";
  import { Journal, type JournalEntry } from "$lib/workbench/journal";
  import ReplHistory from "$lib/workbench/ReplHistory.svelte";
  const journal = new Journal();
  let replay = false;
  function rerun(entry: JournalEntry) {
    if (entry.state === "running") return;
    navigate(entry.command, entry.values);
    replay = true;
  }
  import CommandPanel from "$lib/workbench/CommandPanel.svelte";
  import CommandPalette from "$lib/workbench/CommandPalette.svelte";
  import Hash from "$lib/workbench/Hash.svelte";
  import { commands, commandById } from "$lib/workbench/commands";
  import {
    HttpTransport,
    RequestSlot,
    WorkbenchClient,
  } from "$lib/workbench/client";
  import {
    actor,
    actorTree,
    array,
    definition,
    flattenTree,
    type Actor,
    type Definition,
    type TreeRow,
    type Json,
    type Row,
  } from "$lib/workbench/schema";
  import "@fontsource/inter/latin-400.css";
  import "@fontsource/inter/latin-500.css";
  import "@fontsource/inter/latin-600.css";
  import "@fontsource/jetbrains-mono/latin-400.css";
  import "$lib/app.css";
  let client: WorkbenchClient;
  let ready = false,
    mock = false,
    palette = false,
    help = false,
    settings = false,
    loading = false;
  let endpoint = "",
    token = "",
    error = "",
    selectedDef = "",
    selectedActor = "";
  let definitions: Definition[] = [],
    actors: Actor[] = [],
    tree: TreeRow[] = [];
  let commandId = "find",
    overrides: Record<string, string> = {},
    panelVersion = 0;
  let typeScale = 12;
  let collapsed = new Set<string>();
  let mockDefaults: Record<string, string> = {};
  const slot = new RequestSlot();
  $: command = commandById(commandId);
  $: selectedDefinition = definitions.find((def) => def.hash === selectedDef);
  $: currentActor = actors.find((actor) => actor.id === selectedActor);
  $: defaults = {
    ...mockDefaults,
    query:
      commandId === "actor_sql" && mock
        ? "SELECT * FROM inbox ORDER BY seq"
        : "",
    ...(selectedDefinition
      ? {
          hash: selectedDefinition.hash,
          expected_hash: selectedDefinition.hash,
          name: selectedDefinition.name,
          after: selectedDefinition.hash,
        }
      : {}),
    ...(currentActor
      ? { id: currentActor.id, behavior_hash: currentActor.behavior_hash }
      : {}),
    ...overrides,
  };
  $: actorTabs = [
    "actor_info",
    "actor_inbox",
    "actor_outbox",
    "actor_effects",
    "actor_lineage",
    "actor_dead_letters",
    "actor_validate",
    "actor_promote",
    "actor_send",
  ];
  $: definitionTabs = [
    "view",
    "history",
    "diff",
    "run",
    "dependents",
    "update",
  ];
  $: visibleTree = tree.filter((row) => {
    let parent = row.parent;
    const seen = new Set<string>();
    while (parent) {
      if (collapsed.has(parent)) return false;
      if (seen.has(parent)) return false;
      seen.add(parent);
      parent = actors.find((actor) => actor.id === parent)?.parent ?? null;
    }
    return true;
  });
  async function refresh() {
    loading = true;
    error = "";
    await slot.run(
      async (signal) => {
        const replies = await Promise.all([
          client.call(commandById("find"), { query: "" }, signal),
          client.call(commandById("actor_list"), {}, signal),
          client.call(commandById("actor_tree"), {}, signal),
        ]);
        const definitions = array(replies[0], "find").map(definition);
        const actors = array(replies[1], "actor_list").map(actor);
        return {
          definitions,
          actors,
          tree: flattenTree(actorTree(replies[2]), actors),
        };
      },
      (value) => {
        definitions = value.definitions;
        actors = value.actors;
        tree = value.tree;
        if (!definitions.some((def) => def.hash === selectedDef))
          selectedDef = definitions[0]?.hash ?? "";
        if (!actors.some((actor) => actor.id === selectedActor))
          selectedActor = actors[0]?.id ?? "";
        ready = true;
      },
      (message) => (error = message),
      () => (loading = false),
    );
  }
  function navigate(id: string, values: Record<string, string> = {}) {
    replay = false;
    try {
      commandById(id);
    } catch (problem) {
      error = String(problem);
      return;
    }
    if (values.hash && definitions.some((def) => def.hash === values.hash))
      selectedDef = values.hash;
    if (values.id) selectedActor = values.id;
    commandId = id;
    overrides = values;
    panelVersion++;
    palette = false;
    const url = new URL(location.href);
    url.searchParams.set("panel", id);
    history.replaceState(null, "", url);
  }
  function selectDefinition(def: Definition) {
    selectedDef = def.hash;
    navigate("view", { hash: def.hash });
  }
  function selectActor(actor: Actor) {
    selectedActor = actor.id;
    navigate("actor_info", { id: actor.id });
  }
  function toggle(id: string) {
    const next = new Set(collapsed);
    next.has(id) ? next.delete(id) : next.add(id);
    collapsed = next;
  }
  function completed(body: Row, result: Json) {
    if (command.group === "Definitions") {
      if (["view", "add", "update"].includes(commandId))
        selectedDef = definition(result).hash;
      else if (typeof body.hash === "string") selectedDef = body.hash;
      else if (typeof body.name === "string")
        selectedDef =
          definitions.find((def) => def.name === body.name)?.hash ??
          selectedDef;
    } else if (typeof body.id === "string") selectedActor = body.id;
    if (!command.read && !mock) void refresh();
  }
  async function connect() {
    error = "";
    try {
      if (!mock)
        client = new WorkbenchClient(new HttpTransport(endpoint, token));
      localStorage.setItem(connectionKey, JSON.stringify({ endpoint, token }));
      ready = false;
      panelVersion++;
      settings = false;
      await refresh();
    } catch (problem) {
      error = String(problem);
    }
  }
  async function keyboard(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      palette = !palette;
      return;
    }
    if (palette) return;
    const target = event.target as HTMLElement;
    if (
      target?.closest(
        "input,textarea,select,[contenteditable=true],.cm-editor",
      ) ||
      event.metaKey ||
      event.ctrlKey ||
      event.altKey
    )
      return;
    if (event.key === "?") {
      event.preventDefault();
      help = !help;
    } else if (event.key === "Escape") {
      help = false;
      settings = false;
    } else if (event.key === "+" || event.key === "-") {
      event.preventDefault();
      typeScale = Math.max(
        10,
        Math.min(18, typeScale + (event.key === "+" ? 1 : -1)),
      );
    } else if (event.key === "j" || event.key === "k") {
      const pane = target.closest("[data-pane]");
      const options = [
        ...(pane ?? document).querySelectorAll<HTMLButtonElement>("[data-row]"),
      ];
      if (!options.length) return;
      event.preventDefault();
      const index = options.indexOf(target as HTMLButtonElement);
      const next =
        options[
          Math.max(
            0,
            Math.min(options.length - 1, index + (event.key === "j" ? 1 : -1)),
          )
        ];
      next?.focus();
      next?.scrollIntoView({ block: "nearest" });
    } else if (event.key === "h" || event.key === "l") {
      event.preventDefault();
      const panes = [...document.querySelectorAll<HTMLElement>("[data-pane]")];
      const index = panes.indexOf(target.closest("[data-pane]") as HTMLElement);
      const pane =
        panes[
          Math.max(
            0,
            Math.min(panes.length - 1, index + (event.key === "l" ? 1 : -1)),
          )
        ];
      pane?.querySelector<HTMLElement>('button,input,[tabindex="0"]')?.focus();
    }
  }
  onMount(() => {
    const initialize = async () => {
      const params = new URLSearchParams(location.search);
      mock = params.get("mock") === "1";
      const requestedPanel = params.get("panel") ?? "find";
      try {
        const fragment = location.hash;
        // Remove credentials before parsing or making any request, including failures.
        if (new URLSearchParams(fragment.slice(1)).has("token")) {
          history.replaceState(
            history.state,
            "",
            location.pathname + location.search,
          );
        }
        if (params.has("token")) {
          settings = true;
          throw new Error(
            "Query-string tokens are not accepted; use #token=… because ?token= reaches server logs.",
          );
        }
        const suppliedToken = fragmentToken(fragment);
        if (suppliedToken !== null) {
          endpoint = "";
          token = suppliedToken;
        } else if (!mock) {
          const saved = localStorage.getItem(connectionKey);
          if (saved !== null) {
            const connection = JSON.parse(saved);
            if (
              !connection ||
              typeof connection.endpoint !== "string" ||
              typeof connection.token !== "string"
            )
              throw new Error(
                "Saved connection: expected endpoint and token strings.",
              );
            endpoint = connection.endpoint;
            token = connection.token;
          }
        }
        commandById(requestedPanel);
        commandId = requestedPanel;
        if (mock) {
          const { MockTransport, fixtures } = await import(
            "$lib/workbench/mock"
          );
          const verdict = params.get("verdict") ?? "DivergedAt";
          client = new WorkbenchClient(new MockTransport(verdict));
          mockDefaults = {
            source: fixtures.definitions[0]!.source,
            before: fixtures.definitions[2]!.hash,
            candidate_hash: fixtures.definitions[2]!.hash,
            args: "[42]",
            assertions: JSON.stringify(
              fixtures.verdicts[
                verdict as keyof typeof fixtures.verdicts
              ].assertions.map((assertion) => assertion.query),
            ),
            author: "operator",
            rationale: "Extract increment helper",
            name: "counter",
            old_hash: fixtures.definitions[0]!.hash,
            new_hash: fixtures.definitions[2]!.hash,
            at_seq: "40",
            reason: "shutdown",
            group: "workers",
            query: "SELECT * FROM inbox ORDER BY seq",
            msg: '{"value": 43}',
          };
          selectedActor = "a0-counter";
        }
        if (mock && suppliedToken === null) await refresh();
        else await connect();
        await tick();
      } catch (problem) {
        error = problem instanceof Error ? problem.message : String(problem);
      }
    };
    void initialize();
  });
  onDestroy(() => slot.cancel());
</script>

<svelte:window on:keydown={keyboard} />
<svelte:head
  ><title>loom · definitions & actors{mock ? " · fixture preview" : ""}</title
  ><meta name="color-scheme" content="light dark" /></svelte:head
>
<div class="app-shell" style={`--type-scale:${typeScale}px`}>
  <header class="app-header">
    <Box size={20} class="icon-brand" /><strong class="wordmark">loom</strong
    ><span class="header-path">/</span><span>workspace</span><span
      class="environment">{mock ? "STATIC FIXTURES" : "LOCAL"}</span
    ><button class="palette-trigger" on:click={() => (palette = true)}
      ><Search size={13} /> Commands <kbd>⌘ K</kbd></button
    ><button
      aria-label="Refresh workspace"
      title="Refresh workspace"
      disabled={loading || !client}
      on:click={refresh}><RefreshCw size={14} /></button
    ><button
      aria-label="Connection settings"
      title="Connection settings"
      disabled={mock}
      on:click={() => (settings = !settings)}><Settings2 size={15} /></button
    >
  </header>
  {#if settings}<form
      class="connection-strip"
      on:submit|preventDefault={connect}
    >
      <label
        >API endpoint<input
          bind:value={endpoint}
          placeholder="Same origin"
        /></label
      ><label
        >Bearer token<input
          type="password"
          bind:value={token}
          autocomplete="off"
        /></label
      ><button class="primary" disabled={loading}>Connect</button><span
        class="muted"
        >Saved in this browser. Use #token=…; ?token= is not accepted because it
        reaches server logs.</span
      >
    </form>{/if}
  {#if help}<div class="help-strip">
      <span><kbd>j / k</kbd> Move through rows</span><span
        ><kbd>h / l</kbd> Change panes</span
      ><span><kbd>Enter</kbd> Open</span><span><kbd>⌘ K</kbd> Commands</span
      ><span><kbd>⌘ Enter</kbd> Run form</span><span
        ><kbd>+ / −</kbd> Type scale</span
      ><span><kbd>r</kbd> Rerun focused history entry</span><span
        ><kbd>?</kbd> Hide help</span
      >
    </div>{/if}
  {#if error}<div class="error" role="alert">
      {error}<button
        class="text-control"
        on:click={() => (settings = true)}
        disabled={mock}>Connection settings</button
      >
    </div>{/if}
  <div class="panes">
    <aside
      class="explorer"
      data-pane="explorer"
      aria-label="Workspace explorer"
    >
      <div class="section-bar">
        <FileCode2 size={14} class="icon-code" />
        <h2>Definitions</h2>
        <span class="muted">{definitions.length}</span><button
          class="push"
          aria-label="Add definition"
          on:click={() => navigate("add")}><Plus size={14} /></button
        >
      </div>
      <button class="explorer-action" data-row on:click={() => navigate("find")}
        ><Search size={13} /> Find definitions</button
      >
      {#each definitions as def}<div class="definition-row">
          <button
            class="definition-entry"
            data-row
            aria-current={selectedDef === def.hash &&
            command.group === "Definitions"
              ? "true"
              : undefined}
            on:click={() => selectDefinition(def)}
            ><FileCode2 size={14} class="icon-code" /><span
              ><strong>{def.name}</strong></span
            ></button
          ><Hash value={def.hash} />
        </div>{/each}
      <div class="section-bar actor-heading">
        <Network size={14} class="icon-actor" />
        <h2>Actors</h2>
        <span class="muted">{actors.length}</span><button
          class="push"
          aria-label="Spawn actor"
          on:click={() => navigate("actor_spawn")}><Plus size={14} /></button
        >
      </div>
      <div class="tree-heading">
        <span>Supervision tree</span><span title="Cursor / inbox length"
          >Cursor / Inbox</span
        >
      </div>
      {#each visibleTree as row}<div
          class="tree-entry"
          class:selected={selectedActor === row.id &&
            command.group === "Actors"}
          style={`--depth:${row.depth}`}
        >
          {#if tree.some((child) => child.parent === row.id)}<button
              class="disclosure"
              aria-label={`${collapsed.has(row.id) ? "Expand" : "Collapse"} ${row.id}`}
              aria-expanded={!collapsed.has(row.id)}
              on:click={() => toggle(row.id)}
              >{#if collapsed.has(row.id)}<ChevronRight
                  size={12}
                />{:else}<ChevronDown size={12} />{/if}</button
            >{:else}<span class="disclosure"></span>{/if}
          <button
            class="actor-entry"
            data-row
            aria-current={selectedActor === row.id ? "true" : undefined}
            on:click={() => selectActor(row)}
            ><Circle size={7} class={`status-icon ${row.status}`} /><span
              title={`${row.id} · ${row.status}`}>{row.id}</span
            ><small>{row.cursor} / {row.inbox_len}</small></button
          >
        </div>{/each}
      <button
        class="explorer-action"
        data-row
        on:click={() => navigate("actor_list")}
        >All actors, including forks</button
      >
      <div class="explorer-footer">
        <GitBranch size={12} /><span>Root supervisor → children</span>
      </div>
    </aside>
    <main class="main-pane" data-pane="main" aria-label="Command workspace">
      <nav class="operation-tabs" aria-label={`${command.group} panels`}>
        {#each command.group === "Definitions" ? definitionTabs : actorTabs as id}<button
            aria-current={commandId === id ? "page" : undefined}
            on:click={() => navigate(id)}
            >{id.replace("actor_", "").replaceAll("_", " ")}</button
          >{/each}<button class="push" on:click={() => (palette = true)}
          >All commands</button
        >
      </nav>
      <div class="panel-scroll">
        {#if ready}{#key `${panelVersion}:${commandId}`}<CommandPanel
              {command}
              {client}
              {defaults}
              {mock}
              {navigate}
              {completed}
              {journal}
              {replay}
            />{/key}{:else}<div class="empty">
            {loading
              ? "Reading definitions and actor tree…"
              : "Connect to load the workspace."}
          </div>{/if}
      </div>
      <ReplHistory {journal} {rerun} />
    </main>
  </div>
  <footer class="status-bar">
    <span class="status-light"></span><span
      >{mock
        ? "Static fixture preview · mutations return snapshots"
        : loading
          ? "Reading workspace"
          : ready
            ? "HTTP workspace"
            : "Disconnected"}</span
    ><span class="push">Rust</span><span>UTF-8</span><span>{typeScale}px</span>
  </footer>
</div>
{#if palette}<CommandPalette
    choose={(id) => navigate(id)}
    close={() => (palette = false)}
  />{/if}

<script lang="ts">
  import { onMount } from "svelte";
  import {
    Client,
    format,
    items,
    record,
    type Definition,
    type Actor,
    type LogEvent,
    type Reply,
  } from "$lib/api";
  import Graph from "$lib/Graph.svelte";
  type View = "Session" | "Definitions" | "Actors" | "Dependencies" | "Builds";
  interface Entry {
    id: number;
    source: string;
    mode: string;
    reply?: Reply;
    error?: string;
    ms?: number;
  }
  const views: View[] = [
    "Session",
    "Definitions",
    "Actors",
    "Dependencies",
    "Builds",
  ];
  let view: View = "Session",
    endpoint = "",
    token = "",
    settings = false,
    help = false;
  let source = "1 + 2",
    language = "ts",
    mode = "eval",
    name = "",
    dependencies = "{}",
    session = "",
    busy = false,
    error = "",
    connected = false;
  let entries: Entry[] = [],
    definitions: Definition[] = [],
    actors: Actor[] = [],
    events: LogEvent[] = [];
  let selectedActor = "",
    state: unknown = null,
    buildHash = "",
    build: unknown = null,
    edges: { from: string; to: string }[] = [];
  let socket: WebSocket | undefined;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let reconnectAuthorized = false;
  $: client = new Client(endpoint, token);
  $: sequence = Math.max(
    0,
    ...events.map((e) => e.seq),
    ...entries.map((e) => e.reply?.seq ?? 0),
  );
  async function refresh() {
    try {
      const results = await Promise.all([
        client.command("defs"),
        client.command("actors"),
        client.request("events?limit=100"),
      ]);
      const defs = results[0],
        act = results[1],
        log = results[2];
      if (defs?.ok) definitions = items<Definition>(defs.result, "defs");
      if (act?.ok) actors = items<Actor>(act.result, "actors");
      if (log?.ok) events = items<LogEvent>(log.result, "events");
      error = results
        .filter((r) => !r.ok)
        .map((r) => format(r.error ?? r.diagnostics))
        .join("\n");
    } catch (e) {
      error = String(e);
    }
  }
  function connect() {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (socket) {
      socket.onclose = null;
      socket.close();
    }
    const url = new URL(`${endpoint || location.origin}/v1/stream`);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(url);
    socket.onopen = () =>
      socket?.send(JSON.stringify({ token, after: sequence }));
    socket.onmessage = (event) => {
      try {
        const parsed: unknown = JSON.parse(String(event.data));
        const data = record(parsed);
        if (typeof data.seq === "number" && typeof data.actor === "string") {
          connected = true;
          reconnectAuthorized = true;
          const item = data as unknown as LogEvent;
          events = [...events.filter((e) => e.seq !== item.seq), item]
            .sort((a, b) => a.seq - b.seq)
            .slice(-100);
        } else if (data.ok === false || data.error) {
          connected = false;
          reconnectAuthorized = false;
          error = format(data.error ?? data);
        } else {
          connected = true;
          reconnectAuthorized = true;
        }
      } catch {
        error = "The event stream returned an invalid message.";
      }
    };
    socket.onclose = () => {
      connected = false;
      if (!disposed && reconnectAuthorized)
        reconnectTimer = setTimeout(connect, 1500);
    };
    socket.onerror = () => {
      connected = false;
    };
  }
  function save() {
    reconnectAuthorized = false;
    client = new Client(endpoint, token);
    localStorage.setItem(
      "loom.connection",
      JSON.stringify({ endpoint, token, session }),
    );
    settings = false;
    void refresh();
    connect();
  }
  onMount(() => {
    try {
      const saved = JSON.parse(
        localStorage.getItem("loom.connection") || "{}",
      ) as Record<string, unknown>;
      endpoint = typeof saved.endpoint === "string" ? saved.endpoint : "";
      token = typeof saved.token === "string" ? saved.token : "";
      session = typeof saved.session === "string" ? saved.session : "";
    } catch {
      error = "Saved connection settings could not be read.";
    }
    client = new Client(endpoint, token);
    void refresh();
    connect();
    return () => {
      disposed = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      socket?.close();
    };
  });
  async function submit() {
    if (!source.trim() || busy) return;
    busy = true;
    const id = Date.now();
    const submitted = source;
    entries = [
      ...entries,
      {
        id,
        source: submitted,
        mode: mode === "define" ? `define ${language}` : mode,
      },
    ];
    const start = performance.now();
    try {
      let body: Record<string, unknown> =
        mode === "define"
          ? { lang: language, name, source: submitted }
          : { source: submitted, ...(session ? { session } : {}) };
      if (mode === "define") {
        const deps: unknown = JSON.parse(dependencies);
        if (
          typeof deps !== "object" ||
          deps === null ||
          Array.isArray(deps) ||
          Object.values(deps).some((value) => typeof value !== "string")
        ) {
          throw new Error(
            "Dependencies must be a JSON object mapping import names to definition hashes.",
          );
        }
        body.deps = deps;
      }
      if (mode === "command") {
        const parsed: unknown = JSON.parse(submitted);
        if (
          typeof parsed !== "object" ||
          parsed === null ||
          Array.isArray(parsed)
        )
          throw new Error("Enter a command object with command and args.");
        body = { ...record(parsed), ...(session ? { session } : {}) };
      }
      const reply = await client.request(mode, body);
      entries = entries.map((entry) =>
        entry.id === id
          ? { ...entry, reply, ms: Math.round(performance.now() - start) }
          : entry,
      );
      const result = record(reply.result);
      if (typeof result.session === "string") {
        session = result.session;
        localStorage.setItem(
          "loom.connection",
          JSON.stringify({ endpoint, token, session }),
        );
      }
      await refresh();
    } catch (e) {
      entries = entries.map((entry) =>
        entry.id === id ? { ...entry, error: String(e) } : entry,
      );
    } finally {
      busy = false;
    }
  }
  async function actorState(id: string) {
    selectedActor = id;
    try {
      const reply = await client.command("state", { actor: id });
      if (!reply.ok) throw new Error(format(reply));
      state = reply.result;
    } catch (e) {
      error = String(e);
    }
  }
  async function loadBuild() {
    if (!buildHash) return;
    try {
      const reply = await client.request(
        `builds/${encodeURIComponent(buildHash)}`,
      );
      if (!reply.ok) throw new Error(format(reply));
      const metadata = record(reply.result);
      build =
        typeof metadata.logs_ref === "string" &&
        typeof metadata.logs !== "string"
          ? { ...metadata, logs: await client.text(metadata.logs_ref) }
          : reply.result;
    } catch (e) {
      error = String(e);
    }
  }
  async function loadGraph() {
    view = "Dependencies";
    try {
      const replies = await Promise.all(
        definitions.map(async (def) => {
          const reply = await client.request(
            `graph/deps/${encodeURIComponent(def.hash)}`,
          );
          if (!reply.ok) throw new Error(format(reply));
          return {
            hash: def.hash,
            dependencies: items<string | { hash: string }>(
              reply.result,
              "deps",
            ),
          };
        }),
      );
      edges = replies.flatMap((reply) =>
        reply.dependencies.map((dep) => ({
          from: reply.hash,
          to: typeof dep === "string" ? dep : dep.hash,
        })),
      );
    } catch (e) {
      error = String(e);
    }
  }
  function changeMode() {
    if (mode === "command") source = '{"command":"actors","args":{}}';
    else if (mode === "define")
      source =
        language === "rust"
          ? "#[loom::def]\nfn double(value: i64) -> i64 {\n    value * 2\n}"
          : "export function double(value: number): number {\n  return value * 2;\n}";
    else source = "1 + 2";
  }
</script>

<svelte:head
  ><title>loom · workspace</title><meta
    name="description"
    content="Loom actor workspace. Define, run, and inspect TypeScript and Rust actors."
  /></svelte:head
>
<div class="app">
  <aside>
    <a class="brand" href="/" aria-label="Loom home"
      ><span class="mark">▥</span> loom<span class="version">/ 03</span></a
    >
    <div class="workspace-label">WORKSPACE</div>
    <nav>
      {#each views as item}<button
          class:active={view === item}
          on:click={() =>
            item === "Dependencies" ? loadGraph() : (view = item)}
          ><span class="nav-icon"
            >{item === "Session"
              ? "›_"
              : item === "Definitions"
                ? "ƒ"
                : item === "Actors"
                  ? "◉"
                  : item === "Dependencies"
                    ? "⌘"
                    : "▤"}</span
          >{item}{#if item === "Definitions" || item === "Actors"}<span
              class="count"
              >{item === "Definitions"
                ? definitions.length
                : actors.length}</span
            >{/if}</button
        >{/each}
    </nav>
    <div class="sidebar-bottom">
      <div class="runtime">
        <span class:live={connected} class="dot"></span>{connected
          ? "Event stream connected"
          : "Event stream offline"}
      </div>
      <button on:click={() => (settings = !settings)}
        >Connection settings <span>↗</span></button
      ><button on:click={() => (help = !help)}
        >Keyboard & gestures <span>?</span></button
      >
    </div>
  </aside>
  <main>
    <header>
      <div class="breadcrumb">
        Workspace <span>/</span> <strong>{view}</strong>
      </div>
      <div class="header-right">
        <span class="seq">seq {sequence.toString().padStart(5, "0")}</span
        ><button class="subtle" on:click={refresh}>Refresh</button>
      </div>
    </header>
    <section class="content">
      <div class="title">
        <div>
          <div class="eyebrow">
            {view === "Session"
              ? "THINK IN FUNCTIONS. RUN IN ACTORS."
              : "YOUR WORKSPACE, RECORDED."}
          </div>
          <h1>{view === "Session" ? "A place to work things out." : view}</h1>
          <p>
            {view === "Session"
              ? "TypeScript at the prompt. Rust when you need it. Every effect leaves a trace."
              : view === "Definitions"
                ? "Content-addressed code, across both guest languages."
                : view === "Actors"
                  ? "State is the fold of an actor’s event history."
                  : view === "Dependencies"
                    ? "A shared graph for TypeScript and Rust definitions."
                    : "Component build output, indexed by content hash."}
          </p>
        </div>
        <span class="tag">LOCAL WORKSPACE</span>
      </div>
      {#if settings}<section class="settings panel">
          <h2>Connection</h2>
          <label
            >API endpoint<input
              bind:value={endpoint}
              placeholder="Same origin (default)"
              type="url"
            /></label
          ><label
            >Bearer token<input
              bind:value={token}
              type="password"
              autocomplete="off"
            /></label
          ><label
            >Session actor<input
              bind:value={session}
              placeholder="Created on first eval"
            /></label
          >
          <p>
            Connection settings and the token are stored in this browser’s local
            storage.
          </p>
          <button class="primary" on:click={save}>Save & connect</button>
        </section>{/if}
      {#if help}<div class="notice">
          Run the prompt with ⌘ Enter or Ctrl Enter. In the dependency graph,
          scroll with two fingers to pan and pinch to zoom around the pointer.
          Arrow keys pan; 0 resets the graph.
        </div>{/if}
      {#if error}<div class="error" role="alert">
          {error}<button
            on:click={() => (error = "")}
            aria-label="Dismiss error">×</button
          >
        </div>{/if}
      {#if view === "Session"}<div class="session-grid">
          <div>
            <div class="section-label">
              SESSION <span>{session || "NEW SESSION"}</span>
            </div>
            <div class="history" aria-live="polite">
              {#if entries.length === 0}<div class="welcome panel">
                  <div class="welcome-icon">ƒ</div>
                  <h2>Start with an expression.</h2>
                  <p>
                    Evaluate TypeScript, save a definition, or send a command to
                    an actor. Results and diagnostics appear here.
                  </p>
                  <div class="examples">
                    <button
                      on:click={() => {
                        source = "1 + 2";
                        mode = "eval";
                      }}>1 + 2 <span>↗</span></button
                    ><button
                      on:click={() => {
                        mode = "define";
                        changeMode();
                      }}>Define a function <span>↗</span></button
                    >
                  </div>
                </div>{/if}{#each entries as entry}<article class="entry panel">
                  <div class="entry-label">
                    <span>{entry.mode}</span><span
                      >{entry.ms === undefined
                        ? entry.error
                          ? "failed"
                          : "running…"
                        : `${entry.ms} ms · seq ${entry.reply?.seq}`}</span
                    >
                  </div>
                  <pre class="submitted">{entry.source}</pre>
                  {#if entry.error}<pre
                      class="failed">{entry.error}</pre>{:else if entry.reply}{#if entry.reply.diagnostics?.length}{#each entry.reply.diagnostics as diagnostic}<div
                          class="diagnostic"
                        >
                          <strong>{diagnostic.lang} {diagnostic.code}</strong>
                          <span
                            >{diagnostic.file}:{diagnostic.line}:{diagnostic.col}</span
                          >
                          <p>{diagnostic.message}</p>
                          {#if diagnostic.snippet}<pre>{diagnostic.snippet}</pre>{/if}{#if diagnostic.hint}<p
                              class="hint"
                            >
                              {diagnostic.hint}
                            </p>{/if}
                        </div>{/each}{/if}{#if entry.reply.result !== undefined}<pre
                        class:failed={!entry.reply.ok}
                        class="result">{format(
                          entry.reply.result,
                        )}</pre>{:else if !entry.reply.ok}<pre
                        class="failed">{format(
                          entry.reply.error ?? "Request rejected",
                        )}</pre>{/if}{/if}
                </article>{/each}
            </div>
            <div class="composer panel">
              <div class="composer-bar">
                <select
                  aria-label="Operation"
                  bind:value={mode}
                  on:change={changeMode}
                  ><option value="eval">Evaluate</option><option value="define"
                    >Define</option
                  ><option value="command">Command</option></select
                >{#if mode === "define"}<select
                    aria-label="Guest language"
                    bind:value={language}
                    on:change={changeMode}
                    ><option value="ts">TypeScript</option><option value="rust"
                      >Rust</option
                    ></select
                  ><input
                    aria-label="Definition name"
                    placeholder="Definition name"
                    bind:value={name}
                  />{:else}<span class="language"
                    >{mode === "eval" ? "TypeScript" : "JSON"}</span
                  >{/if}
              </div>
              {#if mode === "define"}
                <label class="dependency-input"
                  >Dependencies
                  <input
                    aria-label="Dependency names and hashes"
                    bind:value={dependencies}
                    spellcheck="false"
                    placeholder={'{"worker":"#hash"}'}
                  />
                </label>
              {/if}
              <textarea
                aria-label="Source code"
                bind:value={source}
                spellcheck="false"
                on:keydown={(event) => {
                  if (
                    event.key === "Enter" &&
                    (event.metaKey || event.ctrlKey)
                  ) {
                    event.preventDefault();
                    void submit();
                  }
                }}
              ></textarea>
              <div class="composer-footer">
                <span>⌘ Enter to run</span><button
                  class="primary"
                  disabled={busy ||
                    !source.trim() ||
                    (mode === "define" && !name.trim())}
                  on:click={submit}
                  >{busy
                    ? "Running…"
                    : mode === "define"
                      ? "Save definition"
                      : "Run"} <span>↗</span></button
                >
              </div>
            </div>
          </div>
          <section class="event-panel">
            <div class="section-label">
              EVENT LOG <span>{events.length}</span>
            </div>
            <div class="event-list">
              {#if !events.length}<div class="empty-small">
                  No events yet.
                  <p>Actor activity will appear here as it happens.</p>
                </div>{/if}{#each [...events].reverse() as event}<div
                  class="event"
                >
                  <span class="event-seq"
                    >{event.seq.toString().padStart(5, "0")}</span
                  ><strong>{event.actor}</strong>
                  <pre>{format(event.event ?? event.event_hash)}</pre>
                </div>{/each}
            </div>
          </section>
        </div>
      {:else if view === "Definitions"}<div class="panel table-wrap">
          <table>
            <thead
              ><tr
                ><th>Definition</th><th>Language</th><th>Content hash</th><th
                  >Component</th
                ></tr
              ></thead
            ><tbody
              >{#each definitions as def}<tr
                  ><td class="definition-name"
                    >{def.name_hint || def.name || "unnamed"}</td
                  ><td
                    ><span class:rust={def.lang === "rust"} class="lang-tag"
                      >{def.lang}</span
                    ></td
                  ><td
                    ><code title={def.hash}>{def.hash.slice(0, 18)}…</code></td
                  ><td
                    >{#if def.component_hash}<button
                        class="text-button"
                        on:click={() => {
                          buildHash = def.component_hash ?? "";
                          view = "Builds";
                          void loadBuild();
                        }}
                        >{def.component_size
                          ? `${(def.component_size / 1024).toFixed(1)} KB`
                          : "View build"} ↗</button
                      >{:else}<span class="muted">Not built</span>{/if}</td
                  ></tr
                >{/each}</tbody
            >
          </table>
          {#if !definitions.length}<div class="empty-small">
              No definitions yet. Save one from the session prompt.
            </div>{/if}
        </div>
      {:else if view === "Actors"}<div class="actor-grid">
          {#each actors as actor}<button
              class:selected={actor.id === selectedActor}
              class="actor-card panel"
              on:click={() => actorState(actor.id)}
              ><div>
                <span class="lang-tag">{actor.lang}</span><span class="muted"
                  >seq {actor.last_seq ?? 0}</span
                >
              </div>
              <h2>{actor.id}</h2>
              <code>{actor.behavior_hash.slice(0, 24)}…</code
              >{#if actor.spawn_ms !== undefined}<p>
                  Spawned in {actor.spawn_ms} ms
                </p>{/if}</button
            >{/each}
        </div>
        {#if !actors.length}<div class="panel empty-small">
            No actors yet. Use the spawn command to create one.
          </div>{/if}{#if selectedActor}<section class="panel state">
            <h2>State · {selectedActor}</h2>
            <pre>{format(state)}</pre>
          </section>{/if}
      {:else if view === "Dependencies"}<Graph {definitions} {edges} />
        <p class="graph-help">
          Two-finger scroll to pan · Pinch to zoom · Keyboard help in the
          sidebar
        </p>
      {:else}<form class="build-search" on:submit|preventDefault={loadBuild}>
          <input
            aria-label="Component hash"
            bind:value={buildHash}
            placeholder="Component hash"
            required
          /><button class="primary">Open build</button>
        </form>
        <section class="panel build-output">
          <div class="section-label">BUILD OUTPUT</div>
          {#if build !== null}<pre>{format(build)}</pre>{:else}<div
              class="empty-small"
            >
              Open a component build to view its logs and metadata.
            </div>{/if}
        </section>{/if}
    </section>
    <footer>
      <span>loom</span> Content addressed. Event sourced.<span
        class="footer-right">TS + Rust / WASM components</span
      >
    </footer>
  </main>
</div>

<style>
  :global(*) {
    box-sizing: border-box;
  }
  :global(body) {
    margin: 0;
    background: #f8faf7;
    color: #20372b;
    font-family:
      Inter,
      -apple-system,
      BlinkMacSystemFont,
      "Segoe UI",
      sans-serif;
    font-size: 14px;
  }
  :global(:root) {
    --line: #dde4dc;
    --muted: #7e8a80;
    --mono: "SFMono-Regular", Consolas, monospace;
  }
  :global(button),
  :global(input),
  :global(select),
  :global(textarea) {
    font: inherit;
  }
  :global(button) {
    cursor: pointer;
  }
  :global(button:disabled) {
    cursor: wait;
    opacity: 0.55;
  }
  :global(button:focus-visible),
  :global(a:focus-visible),
  :global(input:focus-visible),
  :global(textarea:focus-visible),
  :global(select:focus-visible) {
    outline: 2px solid #438568;
    outline-offset: 3px;
  }
  :global(pre) {
    font-family: var(--mono);
    font-size: 12px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    line-height: 1.7;
  }
  :global(code) {
    font-family: var(--mono);
    font-size: 12px;
  }
  .app {
    display: flex;
    min-height: 100vh;
  }
  aside {
    width: 225px;
    flex-shrink: 0;
    padding: 35px 19px 20px;
    background: #f1f4ee;
    border-right: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    position: sticky;
    top: 0;
    height: 100vh;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 30px;
    font-weight: 650;
    letter-spacing: -1.5px;
    color: #264532;
    text-decoration: none;
    padding: 0 13px;
  }
  .mark {
    font-size: 34px;
    line-height: 1;
    color: #6b8660;
  }
  .version {
    margin-left: auto;
    font-size: 11px;
    letter-spacing: 0;
    color: #8c988a;
    font-family: var(--mono);
  }
  .workspace-label {
    font-size: 9px;
    letter-spacing: 1.8px;
    color: #899384;
    margin: 48px 14px 13px;
  }
  nav {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  nav button {
    border: 0;
    background: transparent;
    color: #687762;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 13px;
    border-radius: 7px;
    text-align: left;
    font-size: 13px;
  }
  nav button.active {
    background: #e3eadd;
    color: #28452f;
    font-weight: 600;
  }
  .nav-icon {
    width: 18px;
    font-family: var(--mono);
    font-size: 17px;
  }
  .count {
    margin-left: auto;
    font-size: 10px;
    background: #d8e2d2;
    padding: 2px 6px;
    border-radius: 4px;
  }
  .sidebar-bottom {
    margin-top: auto;
  }
  .sidebar-bottom button {
    width: 100%;
    border: 0;
    background: transparent;
    color: #798574;
    padding: 10px 13px;
    text-align: left;
    font-size: 11px;
  }
  .sidebar-bottom button span {
    float: right;
  }
  .runtime {
    font-size: 10px;
    color: #7c8975;
    padding: 16px 13px;
    border-bottom: 1px solid var(--line);
    margin-bottom: 12px;
  }
  .dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #aab0a6;
    margin-right: 7px;
  }
  .dot.live {
    background: #568c54;
    box-shadow: 0 0 0 3px #e2ebda;
  }
  main {
    min-width: 0;
    flex: 1;
    display: flex;
    flex-direction: column;
  }
  header {
    height: 76px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 42px;
    border-bottom: 1px solid var(--line);
    background: #fbfcfa;
  }
  .breadcrumb {
    font-size: 12px;
    color: #8c978a;
  }
  .breadcrumb span {
    margin: 0 13px;
    color: #c1cabe;
  }
  .breadcrumb strong {
    font-weight: 500;
    color: #4c5f4a;
  }
  .header-right {
    display: flex;
    gap: 23px;
    align-items: center;
  }
  .seq {
    font-size: 10px;
    font-family: var(--mono);
    color: #899683;
  }
  .subtle {
    border: 1px solid var(--line);
    background: transparent;
    border-radius: 5px;
    padding: 6px 10px;
    color: #6a7c65;
    font-size: 11px;
  }
  .content {
    padding: 44px 42px;
    max-width: 1500px;
    width: 100%;
    margin: 0 auto;
    flex: 1;
  }
  .title {
    display: flex;
    justify-content: space-between;
    gap: 20px;
    margin-bottom: 38px;
    align-items: flex-start;
  }
  .eyebrow {
    font-size: 9px;
    font-weight: 600;
    letter-spacing: 1.7px;
    color: #839373;
  }
  h1 {
    font-size: 31px;
    font-weight: 480;
    letter-spacing: -1.1px;
    margin: 12px 0;
  }
  p {
    color: #83907f;
    line-height: 1.7;
    margin: 8px 0;
    font-size: 12px;
  }
  .tag {
    font-size: 8px;
    letter-spacing: 1px;
    white-space: nowrap;
    border: 1px solid #dce4d6;
    border-radius: 4px;
    padding: 6px 8px;
    color: #7c8b70;
    margin-top: 2px;
  }
  .panel {
    background: #fff;
    border: 1px solid var(--line);
    border-radius: 9px;
  }
  .session-grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 250px;
    gap: 28px;
  }
  .section-label {
    font-size: 9px;
    letter-spacing: 1.5px;
    color: #7e8d76;
    display: flex;
    justify-content: space-between;
    padding-bottom: 15px;
  }
  .section-label span {
    letter-spacing: 0.4px;
    color: #a0aa99;
    font-family: var(--mono);
    max-width: 70%;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .welcome {
    padding: 32px;
    margin-bottom: 18px;
  }
  .welcome-icon {
    width: 34px;
    height: 34px;
    background: #eff4eb;
    color: #67805e;
    display: grid;
    place-items: center;
    border-radius: 8px;
    font: 22px Georgia;
  }
  .welcome h2 {
    font-size: 18px;
    font-weight: 500;
    margin: 19px 0 10px;
  }
  .welcome p {
    max-width: 380px;
  }
  .examples {
    display: flex;
    gap: 10px;
    margin-top: 25px;
  }
  .examples button {
    background: #fafbf8;
    border: 1px solid #e4e9df;
    border-radius: 5px;
    font-size: 11px;
    color: #718167;
    padding: 9px 12px;
    flex: 1;
    text-align: left;
  }
  .examples span {
    float: right;
  }
  .composer {
    overflow: hidden;
  }
  .composer-bar {
    display: flex;
    align-items: center;
    padding: 12px 16px;
    gap: 8px;
    border-bottom: 1px solid #edf0e8;
  }
  .composer-bar select {
    border: 0;
    background: #eef3e9;
    color: #5a7350;
    border-radius: 4px;
    font-size: 10px;
    padding: 5px 7px;
  }
  .composer-bar input {
    width: 100%;
    min-width: 0;
    font-size: 11px;
    border: 0;
    background: transparent;
  }
  .language {
    margin-left: auto;
    font-size: 10px;
    color: #92a08b;
  }
  .composer textarea {
    resize: vertical;
    width: 100%;
    min-height: 155px;
    border: 0;
    padding: 20px;
    background: transparent;
    color: #395834;
    font: 12px/1.8 var(--mono);
    display: block;
  }
  .dependency-input {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 16px;
    border-bottom: 1px solid var(--line);
    font-size: 10px;
    color: var(--muted);
  }
  .dependency-input input {
    flex: 1;
    min-width: 0;
    font: 11px var(--mono);
    border: 0;
    background: transparent;
    color: #526849;
  }
  .composer-footer {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 13px 16px;
    background: #fcfdfb;
    border-top: 1px solid #edf0e8;
  }
  .composer-footer > span {
    font-size: 10px;
    color: #9aa491;
  }
  .primary {
    background: #345c3d;
    border: 1px solid #345c3d;
    color: white;
    border-radius: 5px;
    font-size: 11px;
    padding: 9px 15px;
  }
  .primary span {
    margin-left: 18px;
  }
  .event-panel {
    border-left: 1px solid var(--line);
    padding-left: 24px;
  }
  .event-list {
    max-height: 650px;
    overflow: auto;
  }
  .empty-small {
    padding: 27px 20px;
    color: #889681;
    font-size: 12px;
    text-align: center;
  }
  .event-panel .empty-small {
    padding: 40px 5px;
    text-align: left;
  }
  .event-panel .empty-small p {
    font-size: 11px;
  }
  .event {
    padding: 15px 0;
    border-bottom: 1px solid var(--line);
    overflow: hidden;
  }
  .event-seq {
    font-size: 9px;
    font-family: var(--mono);
    color: #9caa94;
    margin-right: 8px;
  }
  .event strong {
    font-size: 11px;
    font-weight: 500;
    overflow-wrap: anywhere;
  }
  .event pre {
    font-size: 10px;
    color: #84907c;
    max-height: 110px;
    overflow: auto;
  }
  .entry {
    margin-bottom: 14px;
    overflow: hidden;
  }
  .entry-label {
    padding: 11px 16px;
    display: flex;
    justify-content: space-between;
    color: #8e9b87;
    font-size: 10px;
    border-bottom: 1px solid #edf0e8;
  }
  .submitted {
    padding: 14px 18px;
    margin: 0;
    color: #5a7050;
  }
  .result {
    background: #f5f8f1;
    border-top: 1px solid #e6eddc;
    padding: 14px 18px;
    margin: 0;
    color: #395c31;
  }
  .failed,
  .diagnostic {
    color: #a34535;
    background: #fff6f2;
    padding: 16px;
    margin: 0;
  }
  .diagnostic {
    font-size: 12px;
  }
  .diagnostic p {
    color: inherit;
  }
  .diagnostic span {
    font-size: 10px;
  }
  .hint {
    font-style: italic;
  }
  .error,
  .notice {
    padding: 16px;
    border: 1px solid #e7caba;
    background: #fff4ec;
    color: #92573b;
    border-radius: 7px;
    margin-bottom: 20px;
    font-size: 12px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .error button {
    float: right;
    background: none;
    border: 0;
    color: inherit;
  }
  .notice {
    border-color: #d7e1cf;
    background: #f0f5e9;
    color: #647857;
  }
  .settings {
    padding: 22px;
    margin-bottom: 25px;
  }
  .settings label {
    display: block;
    margin: 12px 0;
    font-size: 12px;
  }
  .settings input {
    display: block;
    width: 100%;
    max-width: 500px;
    margin-top: 7px;
    padding: 9px;
    border: 1px solid var(--line);
    border-radius: 4px;
  }
  .settings h2,
  .state h2 {
    font-size: 16px;
    font-weight: 500;
  }
  .table-wrap {
    overflow: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    text-align: left;
  }
  th {
    font-size: 9px;
    font-weight: 500;
    letter-spacing: 1px;
    color: #87957f;
    background: #fafcf7;
    padding: 16px 20px;
  }
  td {
    padding: 20px;
    border-top: 1px solid var(--line);
    font-size: 12px;
  }
  .definition-name {
    font-weight: 500;
  }
  .lang-tag {
    background: #e9f1ed;
    color: #4c7c65;
    font-size: 10px;
    padding: 4px 7px;
    border-radius: 4px;
  }
  .lang-tag.rust {
    background: #fbede4;
    color: #a57050;
  }
  .text-button {
    background: none;
    border: 0;
    color: #5b7850;
    padding: 0;
    font-size: 11px;
  }
  .muted {
    color: #94a08c;
    font-size: 11px;
  }
  .actor-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(230px, 1fr));
    gap: 16px;
  }
  .actor-card {
    padding: 20px;
    text-align: left;
    color: inherit;
    min-width: 0;
  }
  .actor-card.selected {
    border-color: #69875a;
  }
  .actor-card > div {
    display: flex;
    justify-content: space-between;
  }
  .actor-card h2 {
    font-size: 14px;
    font-weight: 500;
    overflow-wrap: anywhere;
    margin: 22px 0 10px;
  }
  .actor-card code {
    font-size: 10px;
    color: #93a087;
  }
  .state {
    margin-top: 20px;
    padding: 20px;
  }
  .graph-help {
    font-size: 10px;
    text-align: right;
  }
  .build-search {
    display: flex;
    gap: 12px;
    margin-bottom: 20px;
  }
  .build-search input {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line);
    padding: 12px;
    border-radius: 5px;
    font: 12px var(--mono);
  }
  .build-output {
    padding: 20px;
    min-height: 280px;
  }
  .build-output pre {
    color: #526849;
  }
  footer {
    border-top: 1px solid var(--line);
    padding: 17px 42px;
    font-size: 9px;
    color: #a0aa98;
  }
  footer > span:first-child {
    font-size: 12px;
    font-weight: 600;
    color: #7e8f74;
    margin-right: 13px;
  }
  .footer-right {
    float: right;
    font-family: var(--mono);
    font-size: 9px;
  }
  @media (min-width: 1500px) {
    .content {
      padding-top: 60px;
    }
    .session-grid {
      grid-template-columns: minmax(0, 1fr) 310px;
    }
  }
  @media (max-width: 1050px) {
    aside {
      width: 185px;
    }
    .content {
      padding: 30px 25px;
    }
    .session-grid {
      grid-template-columns: 1fr;
    }
    .event-panel {
      border-left: 0;
      padding-left: 0;
      margin-top: 8px;
    }
    .event-list {
      max-height: 250px;
    }
    .tag {
      display: none;
    }
    header {
      padding: 0 25px;
    }
  }
  @media (max-width: 650px) {
    .app {
      display: block;
    }
    aside {
      width: 100%;
      height: auto;
      position: static;
      padding: 16px;
      border-right: 0;
      border-bottom: 1px solid var(--line);
    }
    .brand {
      font-size: 25px;
    }
    .workspace-label,
    .sidebar-bottom .runtime {
      display: none;
    }
    nav {
      flex-direction: row;
      overflow-x: auto;
      margin-top: 16px;
    }
    nav button {
      padding: 9px;
      font-size: 11px;
      white-space: nowrap;
    }
    .nav-icon,
    .count {
      display: none;
    }
    .sidebar-bottom {
      display: flex;
      margin-top: 8px;
    }
    .sidebar-bottom button {
      font-size: 10px;
    }
    header {
      height: 55px;
    }
    .content {
      padding: 28px 18px;
    }
    h1 {
      font-size: 25px;
    }
    .title {
      margin-bottom: 26px;
    }
    .welcome {
      padding: 24px;
    }
    .header-right {
      gap: 10px;
    }
    footer {
      padding: 16px 18px;
    }
    .footer-right {
      display: none;
    }
  }
</style>

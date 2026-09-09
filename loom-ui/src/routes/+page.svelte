<script lang="ts">
  import { onMount } from "svelte";
  import {
    GitBranch,
    Terminal,
    AlignLeft,
    Circle,
    Database,
    Settings,
    RefreshCw,
    X,
    HelpCircle,
    ChevronRight,
  } from "lucide-svelte";
  import {
    Client,
    AuthenticationError,
    items,
    record,
    resultOf,
    type Definition,
    type Actor,
    type LogEvent,
    type Reply,
  } from "$lib/api";
  import type { Entry } from "$lib/journal";
  import SessionJournal from "$lib/SessionJournal.svelte";
  import Composer from "$lib/Composer.svelte";
  import ActorBrowser from "$lib/ActorBrowser.svelte";
  import DefinitionBrowser from "$lib/DefinitionBrowser.svelte";
  import EffectsBrowser from "$lib/EffectsBrowser.svelte";
  import CasBrowser from "$lib/CasBrowser.svelte";
  import "@fontsource/jetbrains-mono/400.css";
  import "$lib/app.css";
  type View = "Session" | "Definitions" | "Actors" | "CAS" | "Effects";
  const views: View[] = ["Session", "Definitions", "Actors", "CAS", "Effects"];
  let selectedActor = "";
  let view: View = "Session",
    endpoint = "",
    token = "",
    session = "",
    settings = false,
    help = false,
    error = "",
    connected = false;
  let definitions: Definition[] = [],
    actors: Actor[] = [],
    events: LogEvent[] = [],
    entries: Entry[] = [];
  let mode = "eval",
    language = "ts",
    source = "",
    name = "",
    dependencies = "{}",
    busy = false,
    refreshing = false;
  let inspectHash = "",
    socket: WebSocket | undefined,
    reconnectTimer: ReturnType<typeof setTimeout> | undefined,
    disposed = false,
    reconnectAuthorized = false;
  let connecting = false;
  function unauthorized(problem: AuthenticationError) {
    connected = false;
    reconnectAuthorized = false;
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (socket) { socket.onclose = null; socket.close(); socket = undefined; }
    settings = true;
    error = problem.message;
  }
  function makeClient(endpoint: string, token: string) {
    const candidate = new Client(endpoint, token, problem => { if (candidate === client) unauthorized(problem); });
    return candidate;
  }
  let client = makeClient("", "");
  $: sequence = Math.max(
    0,
    ...events.map((event) => event.seq),
    ...entries.map((entry) => entry.reply?.seq ?? 0),
  );
  async function refresh() {
    const current = client;
    refreshing = true;
    try {
      const results = await Promise.all([
        current.command("defs"),
        current.command("actors"),
        current.request("events?limit=1000"),
      ]);
      if (current !== client) return;
      definitions = items<Definition>(resultOf(results[0]!), "defs");
      actors = items<Actor>(resultOf(results[1]!), "actors");
      const received = items<LogEvent>(resultOf(results[2]!), "events");
      const combined = new Map<number, LogEvent>();
      for (const event of [...received, ...events])
        combined.set(event.seq, event);
      events = [...combined.values()]
        .sort((a, b) => a.seq - b.seq)
        .slice(-1000);
      error = "";
    } catch (e) {
      if (current === client) error = e instanceof Error ? e.message : String(e);
    } finally {
      if (current === client) refreshing = false;
    }
  }
  function connect() {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (socket) {
      socket.onclose = null;
      socket.close();
    }
    let url: URL;
    try {
      url = new URL(`${client.endpoint || location.origin}/v1/stream`);
    } catch {
      error = "Enter a valid API endpoint URL.";
      return;
    }
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const streamToken = client.token;
    const current = new WebSocket(url);
    socket = current;
    current.onopen = () =>
      current.send(JSON.stringify({ token: streamToken, after: sequence }));
    current.onmessage = (event) => {
      if (socket !== current) return;
      try {
        const data = record(JSON.parse(String(event.data)));
        if (data.ok === false || data.error) {
          unauthorized(new AuthenticationError());
          return;
        }
        connected = true;
        reconnectAuthorized = true;
        if (typeof data.seq === "number" && typeof data.actor === "string") {
          const item = data as unknown as LogEvent;
          events = [...events.filter((event) => event.seq !== item.seq), item]
            .sort((a, b) => a.seq - b.seq)
            .slice(-1000);
        }
      } catch {
        error = "The event stream returned an invalid message.";
      }
    };
    current.onclose = () => {
      if (socket !== current) return;
      connected = false;
      if (!disposed && reconnectAuthorized)
        reconnectTimer = setTimeout(connect, 1500);
    };
    current.onerror = () => {
      if (socket === current) connected = false;
    };
  }
  function persist() {
    localStorage.setItem(
      "loom.connection",
      JSON.stringify({ endpoint, token, session }),
    );
  }
  async function save() {
    if (connecting) return;
    connecting = true;
    const candidate = makeClient(endpoint.trim(), token.trim());
    try {
      resultOf(await candidate.command("actors"));
      client.dispose();
      client = candidate;
      endpoint = candidate.endpoint;
      token = candidate.token;
      reconnectAuthorized = false;
      persist();
      settings = false;
      error = "";
      await refresh();
      if (!settings) connect();
    } catch (problem) {
      candidate.dispose();
      settings = true;
      error = problem instanceof Error ? problem.message : String(problem);
    } finally { connecting = false; }
  }
  onMount(() => {
    try {
      const data = record(
        JSON.parse(localStorage.getItem("loom.connection") || "{}"),
      );
      endpoint = typeof data.endpoint === "string" ? data.endpoint : "";
      token = typeof data.token === "string" ? data.token : "";
      session = typeof data.session === "string" ? data.session : "";
    } catch {
      error = "Saved connection settings could not be read.";
    }
    client.dispose();
    client = makeClient(endpoint, token);
    if (token) {
      void refresh();
      connect();
    } else settings = true;
    return () => {
      disposed = true;
      client.dispose();
      if (reconnectTimer) clearTimeout(reconnectTimer);
      socket?.close();
    };
  });
  function inspect(hash: string) {
    inspectHash = hash;
    view = "CAS";
  }
  function navigate(next: View) {
    view = next;
    if (next === "CAS") inspectHash = "";
    if (next !== "Session") void refresh();
  }
  async function submit() {
    if (busy || !source.trim() || (mode === "define" && !name.trim())) return;
    busy = true;
    const id = Date.now(),
      submitted = source,
      operation = mode;
    const entry: Entry = {
      id,
      source: submitted,
      mode: operation === "define" ? `define ${language}` : operation,
      ...(operation === "define" ? { name } : {}),
    };
    entries = [...entries, entry];
    const start = performance.now();
    try {
      let body: Record<string, unknown> = {
        source: submitted,
        ...(session ? { session } : {}),
      };
      if (operation === "define") {
        const deps: unknown = JSON.parse(dependencies);
        if (
          !deps ||
          typeof deps !== "object" ||
          Array.isArray(deps) ||
          Object.values(deps).some((value) => typeof value !== "string")
        )
          throw new Error(
            "Dependencies must map import names to definition hashes.",
          );
        body = { lang: language, name, source: submitted, deps };
      } else if (operation === "command") {
        const command: unknown = JSON.parse(submitted);
        if (typeof record(command).command !== "string")
          throw new Error("Enter an object with command and args.");
        body = { ...record(command), ...(session ? { session } : {}) };
      }
      const reply = await client.request(operation, body);
      entries = entries.map((item) =>
        item.id === id
          ? { ...item, reply, ms: Math.round(performance.now() - start) }
          : item,
      );
      const returned = record(reply.result);
      if (typeof returned.session === "string") {
        session = returned.session;
        persist();
      }
      await refresh();
    } catch (e) {
      entries = entries.map((item) =>
        item.id === id
          ? {
              ...item,
              error: String(e),
              ms: Math.round(performance.now() - start),
            }
          : item,
      );
    } finally {
      busy = false;
    }
  }
  async function runCommand(
    command: string,
    args: Record<string, unknown>,
  ): Promise<Reply> {
    const start = performance.now();
    const reply = await client.command(command, args);
    entries = [
      ...entries,
      {
        id: Date.now(),
        source: JSON.stringify({ command, args }, null, 2),
        mode: "command",
        reply,
        ms: Math.round(performance.now() - start),
      },
    ];
    await refresh();
    return reply;
  }
  function call(definition: Definition) {
    view = "Session";
    mode = "command";
    source = JSON.stringify(
      { command: "call", args: { hash: definition.hash, args: [] } },
      null,
      2,
    );
  }
</script>

<svelte:head
  ><title>loom · workspace</title><meta
    name="description"
    content="A live journal for Loom sessions, definitions, actors, and content."
  /><meta name="color-scheme" content="light dark" /></svelte:head
>
<header class="workspace-header">
  <a href="/" class="wordmark">loom</a><span class="header-divider">/</span>
  <div class="workspace-identity">
    <GitBranch size={14} /><span>workspace</span>
  </div>
  <div class="header-controls">
    <span class="connection-status"
      ><span class:connected class="status-dot"></span>{connected
        ? "Connected"
        : "Offline"}</span
    ><button
      title="Refresh workspace"
      aria-label="Refresh workspace"
      disabled={refreshing}
      on:click={refresh}><RefreshCw size={14} /></button
    ><button
      title="Keyboard & gestures"
      aria-label="Keyboard & gestures"
      on:click={() => (help = !help)}><HelpCircle size={14} /></button
    ><button
      title="Connection settings"
      aria-label="Connection settings"
      on:click={() => (settings = !settings)}><Settings size={14} /></button
    >
  </div>
</header>
<div class="workspace">
  <nav class="primary-nav" aria-label="Workspace views">
    {#each views as item}<button
        aria-current={view === item ? "page" : undefined}
        on:click={() => navigate(item)}
        >{#if item === "Session"}<Terminal
            size={14}
          />{:else if item === "Definitions"}<AlignLeft
            size={14}
          />{:else if item === "Actors"}<Circle size={13} />{:else}<Database
            size={14}
          />{/if}<span>{item}</span
        >{#if item === "Definitions" || item === "Actors"}<small
            >{item === "Definitions"
              ? definitions.length
              : actors.length}</small
          >{/if}</button
      >{/each}<span class="sequence">seq {sequence}</span>
  </nav>
  <main>
    {#if settings}<section class="connection-panel">
        <div class="panel-heading">
          <h2>Connect to Loom</h2>
          <button
            aria-label="Close connection settings"
            on:click={() => (settings = false)}><X size={14} /></button
          >
        </div>
        <form on:submit|preventDefault={save}>
          <label
            >API endpoint<input
              bind:value={endpoint}
              placeholder="Same origin"
              type="url"
            /></label
          ><label
            >Bearer token<input
              bind:value={token}
              type="password"
              autocomplete="off"
              data-1p-ignore
              data-lpignore="true"
              placeholder="Your local daemon token"
            /></label
          >
          <details>
            <summary><ChevronRight size={11} /> Session identity</summary><label
              >Session<input
                bind:value={session}
                placeholder="Created on first evaluation"
              /></label
            >
          </details>
          <div class="connection-footer">
            <p>Saved in this browser’s local storage.</p>
            <button class="primary" disabled={connecting}>{connecting ? "Checking…" : "Connect"}</button>
          </div>
        </form>
      </section>{/if}{#if help}<div class="help-panel">
        <p>
          <strong>Prompt</strong> ⌘ Enter or Ctrl Enter runs the current input.
        </p>
        <p>
          <strong>Dependency graph</strong> Two-finger scroll pans. Pinch zooms around
          the pointer. Arrow keys pan; 0 resets.
        </p>
      </div>{/if}{#if error}<div class="error" role="alert">
        <span>{error}</span><button
          aria-label="Dismiss error"
          on:click={() => (error = "")}><X size={13} /></button
        >
      </div>{/if}
    {#key client}{#if view === "Session"}<SessionJournal
        {events}
        {entries}
        {inspect}
        loadSource={(hash) => client.text(hash)}
      /><Composer
        bind:mode
        bind:language
        bind:source
        bind:name
        bind:dependencies
        {busy}
        {submit}
      />
      <div class="session-footer">
        <span>{session ? `Session ${session}` : "New session"}</span><span
          >TypeScript prompt · Rust definitions</span
        >
      </div>{:else if view === "Definitions"}<DefinitionBrowser
        {client}
        {definitions}
        {events}
        {inspect}
        {call}
      />{:else if view === "Effects"}<EffectsBrowser {client} {events} {inspect} actor={(id) => { selectedActor = id; view = "Actors"; }} />{:else if view === "Actors"}<ActorBrowser
        initialActor={selectedActor}
        {client}
        {actors}
        {definitions}
        {inspect}
        {runCommand}
        loadSource={(hash) => client.text(hash)}
      />{:else}<CasBrowser {client} initialHash={inspectHash} />{/if}{/key}
  </main>
  <footer class="workspace-footer">
    <span>loom</span><span>Content addressed · Event sourced</span>
  </footer>
</div>

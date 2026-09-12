<script lang="ts">
  import { onMount } from "svelte";
  import { X } from "lucide-svelte";
  import WorkspaceNavigation from "$lib/WorkspaceNavigation.svelte";
  import WorkspaceHeader from "$lib/WorkspaceHeader.svelte";
  import type { WorkspaceView } from "$lib/workspace-view";
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
  import ConnectionForm from "$lib/ConnectionForm.svelte";
  import Composer from "$lib/Composer.svelte";
  import ActorBrowser from "$lib/ActorBrowser.svelte";
  import DefinitionBrowser from "$lib/DefinitionBrowser.svelte";
  import EffectsBrowser from "$lib/EffectsBrowser.svelte";
  import CasBrowser from "$lib/CasBrowser.svelte";
  import "@fontsource/jetbrains-mono/400.css";
  import "$lib/app.css";
  let selectedActor = "";
  let view: WorkspaceView = "Session",
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
  let authenticated = false;
  function unauthorized(problem: AuthenticationError) {
    authenticated = false;
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
    if (connecting || !token.trim()) return;
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
      authenticated = true;
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
    if (token) void save();
    else settings = true;
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
  function navigate(next: WorkspaceView) {
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
      mode: operation === "define" ? "define rust" : operation,
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
        body = { lang: "rust", name, source: submitted, deps };
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
{#if !authenticated}
  <main class="authentication-gate">
    <section class="connection-panel" aria-labelledby="connect-title" aria-busy={connecting}>
      <a href="/" class="wordmark">loom</a>
      <h1 id="connect-title">Connect to your workspace</h1>
      <p class="gate-intro">Enter your token to access sessions, actors, and the content store.</p>
      {#if error}<p class="gate-error" role="alert">{error}</p>{/if}
      <ConnectionForm bind:endpoint bind:token bind:session {connecting} submit={save} />
    </section>
  </main>
{:else}
<WorkspaceHeader {connected} {refreshing} {refresh}
  toggleHelp={() => help = !help} toggleSettings={() => settings = !settings} />
<div class="workspace">
  <WorkspaceNavigation {view} {sequence} {navigate}
    definitionCount={definitions.length} actorCount={actors.length} />
  <main>
    {#if settings}<section class="connection-panel">
        <div class="panel-heading">
          <h2>Connect to Loom</h2>
          <button
            aria-label="Close connection settings"
            on:click={() => (settings = false)}><X size={14} /></button
          >
        </div>
        <ConnectionForm bind:endpoint bind:token bind:session {connecting} submit={save} />
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
      {client}
        {events}
        {entries}
        {inspect}
        loadSource={(hash) => client.text(hash)}
      /><Composer
        bind:mode
        bind:source
        bind:name
        bind:dependencies
        {busy}
        {submit}
      />
      <div class="session-footer">
        <span>{session ? `Session ${session}` : "New session"}</span><span
          >Rust prompt · Rust definitions</span
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

{/if}

<style>
  .authentication-gate { min-height:100svh; display:grid; place-items:center; padding:28px 20px; }
  .authentication-gate .connection-panel { width:100%; max-width:440px; margin:0; padding:32px; }
  .authentication-gate .wordmark { display:inline-block; margin-bottom:28px; }
  .authentication-gate h1 { font-size:22px; font-weight:500; letter-spacing:-.6px; margin:0 0 8px; }
  .gate-intro { color:var(--muted); line-height:1.7; margin:0 0 28px; }
  .gate-error { color:var(--error); margin:0 0 20px; }
</style>

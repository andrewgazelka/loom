<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { Client, record, resultOf } from "$lib/api";
  import { bind, type Binding } from "$lib/bind/bind";
  import { SocketStream } from "$lib/bind/stream";
  import type { EventPayload } from "$lib/bind/patch";
  import "@fontsource/inter/latin-400.css";
  import "@fontsource/inter/latin-500.css";
  import "$lib/app.css";

  let endpoint = "";
  let token = "";
  let actor = "";
  let table = "";
  let template = "";
  let orderBy = "";
  let container: HTMLDivElement;
  let error = "";
  let busy = false;
  let viewId = "";
  let generation = 0;
  interface Session { client: Client; stream: SocketStream; binding: Binding; id: string; source: string }
  // disconnect() closes the ws subscriber and stops the ephemeral view owned by this page.
  let session: Session | undefined;

  onMount(() => {
    try {
      const stored = localStorage.getItem("loom.connection");
      if (stored) {
        const connection = record(JSON.parse(stored));
        if (typeof connection.endpoint === "string") endpoint = connection.endpoint;
        if (typeof connection.token === "string") token = connection.token;
      }
    } catch (cause) { error = `Connection settings: ${cause}`; }
  });
  onDestroy(() => { void disconnect(); });

  async function stop(client: Client, id: string) {
    try { resultOf(await client.request("command", { command: "stop", args: { id, reason: "normal" } })); }
    finally { client.dispose(); }
  }
  async function disconnect() {
    generation++;
    const previous = session;
    session = undefined;
    viewId = "";
    if (!previous) return;
    previous.stream.close();
    previous.binding.destroy();
    try { await stop(previous.client, previous.id); }
    catch (cause) { error = `Stop view ${previous.id}: ${cause}`; }
  }
  async function send(owned: Session, rowKey: string, name: string, payload: EventPayload) {
    const key = `browser:${crypto.randomUUID()}`;
    const row = owned.binding.rows.get(rowKey);
    if (row) owned.binding.pending(rowKey, row.tree, key);
    try {
      const reply = await owned.client.request("command", {
        command: "send", args: { id: owned.source, key, msg: { type: name, key: rowKey, payload } },
      });
      const failure = record(reply.result);
      if (!reply.ok && failure.code === "actor_message_failed" && typeof failure.cause === "string") {
        if (session === owned) owned.binding.receive({ type: "dead_letter", key,
          error: `actor ${owned.source} seq ${failure.seq}: ${failure.cause}` });
      } else resultOf(reply);
    } catch (cause) {
      // A lost HTTP acknowledgement is not a data verdict; a later delta/snapshot owns reconciliation.
      if (session === owned) error = `Message ${key}: ${cause}. Outcome unknown; reconnect for a fresh snapshot.`;
    }
  }
  async function connect() {
    busy = true;
    error = "";
    await disconnect();
    const revision = generation;
    const source = actor.trim();
    const client = new Client(endpoint.trim(), token);
    let created: string | undefined;
    try {
      const result = record(resultOf(await client.request("command", {
        command: "view", args: { actor: source, table: table.trim(), template: template.trim(),
          order_by: orderBy.split(",").map((value) => value.trim()).filter(Boolean) },
      })));
      if (typeof result.id !== "string") throw new Error("view: response lacks actor id");
      created = result.id;
      if (typeof result.cap !== "string") throw new Error(`view ${created}: response lacks opaque inspection capability`);
      if (revision !== generation) { await stop(client, created); return; }
      const stream = new SocketStream({ endpoint: endpoint.trim(), token,
        subscription: { actor: created, table: "tree", cap: result.cap },
        onError: (cause) => { if (revision === generation) error = cause.message; },
      });
      const binding: Binding = bind(container, stream, {
        onEvent: (key, name, payload) => { if (session?.binding === binding) void send(session, key, name, payload); },
        onError: (cause) => { if (revision === generation) error = cause.message; },
      });
      session = { client, stream, binding, id: created, source };
      viewId = created;
    } catch (cause) {
      if (revision === generation) error = String(cause);
      if (created) {
        try { await stop(client, created); }
        catch (cleanup) { if (revision === generation) error += `; stop view ${created}: ${cleanup}`; }
      } else client.dispose();
    } finally { if (revision === generation) busy = false; }
  }
</script>

<svelte:head><title>Actor view · Loom</title></svelte:head>
<main class="view-page">
  <header><a href="/">Loom</a><h1>Actor view</h1><span>{viewId || "Choose a source and template"}</span></header>
  <form onsubmit={(event) => { event.preventDefault(); void connect(); }}>
    <fieldset disabled={busy}>
      <label>Endpoint<input bind:value={endpoint} placeholder="Page origin" autocomplete="url" /></label>
      <label>Bearer token<input type="password" bind:value={token} autocomplete="off" /></label>
      <label>Source actor<input bind:value={actor} required placeholder="Actor id" /></label>
      <label>Table<input bind:value={table} required placeholder="counter" /></label>
      <label>Template<input bind:value={template} required placeholder="Definition hash" /></label>
      <label>Order by<input bind:value={orderBy} placeholder="Columns, separated by commas" /></label>
      <button type="submit">{busy ? "Opening…" : "Open view"}</button>
      {#if viewId}<button type="button" onclick={() => { void disconnect(); }}>Disconnect</button>{/if}
    </fieldset>
  </form>
  {#if error}<p class="view-error" role="alert">{error}</p>{/if}
  <div class="view-trees" bind:this={container} aria-label="Rendered actor rows"></div>
</main>

<style>
  .view-page { display: flex; flex-direction: column; min-height: 100dvh; font-family: Inter, sans-serif; }
  header { display: flex; align-items: baseline; gap: 18px; padding: 14px 20px; border-bottom: 1px solid var(--line); }
  h1 { font-size: 16px; margin: 0; font-weight: 500; }
  header span { color: var(--muted); overflow-wrap: anywhere; }
  form { padding: 16px 20px; border-bottom: 1px solid var(--line); }
  fieldset { border: 0; padding: 0; margin: 0; display: flex; flex-wrap: wrap; align-items: end; gap: 12px; }
  label { display: flex; flex-direction: column; gap: 4px; min-width: 170px; flex: 1; }
  button { border: 1px solid var(--line); border-radius: 5px; padding: 6px 10px; }
  .view-error { color: var(--error); margin: 12px 20px; white-space: pre-wrap; }
  .view-trees { padding: 20px; overflow: auto; flex: 1; }
  .view-trees :global([data-pending]) { opacity: 0.6; }
</style>

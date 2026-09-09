<script lang="ts">
  import RowPreview from "./RowPreview.svelte";
  import Select from "./Select.svelte";
  import {
    Circle,
    ArrowLeft,
    GitFork,
    ArrowUp,
    Send,
    Plus,
    ChevronRight,
    Activity,
  } from "lucide-svelte";
  import {
    Client,
    resultOf,
    record,
    short,
    type Actor,
    type Definition,
    type Reply,
  } from "./api";
  import { definitionName, reference } from "./journal";
  import ValueView from "./ValueView.svelte";
  import EvaluationView from "./EvaluationView.svelte";
  import EventCard from "./EventCard.svelte";
  import { eventRow } from "./journal";
  import type { LogEvent } from "./api";
  let query = "";
  export let initialActor = "";
  let openedInitial = "";
  $: if(initialActor && initialActor !== openedInitial && actors.some(actor => actor.id === initialActor)) { openedInitial = initialActor; void open(actors.find(actor => actor.id === initialActor)!); }
  export let client: Client;
  export let actors: Actor[] = [];
  export let definitions: Definition[] = [];
  export let inspect: (hash: string) => void;
  export let runCommand: (
    command: string,
    args: Record<string, unknown>,
  ) => Promise<Reply>;
  export let loadSource: (hash: string) => Promise<string>;
  let selected: Actor | null = null,
    state: unknown = null,
    logs: LogEvent[] = [],
    busy = false,
    error = "",
    tab = "state";
  let action = "",
    behavior = "",
    message = "null",
    actionResult: unknown = undefined;
  async function open(actor: Actor) {
    selected = actor;
    busy = true;
    error = "";
    try {
      const result = await Promise.all([
        client.command("state", { actor: actor.id }),
        client.command("events", { actor: actor.id, limit: 1000 }),
      ]);
      state = resultOf(result[0]!);
      const data = resultOf(result[1]!);
      logs = Array.isArray(data) ? (data as LogEvent[]) : [];
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
  async function execute() {
    busy = true;
    error = "";
    try {
      const args: Record<string, unknown> =
        action === "spawn"
          ? { hash: behavior, initial: JSON.parse(message) }
          : action === "send"
            ? { actor: selected?.id, msg: JSON.parse(message) }
            : action === "upgrade"
              ? { actor: selected?.id, hash: behavior }
              : { actor: selected?.id };
      actionResult = resultOf(await runCommand(action === "upgrade" ? "actor.upgrade" : action, args));
      action = "";
      if (selected) await open(selected);
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
  $: evaluated = record(state).type === "evaluated";
</script>

<div class="section-heading">
  <div>
    <h1>Actors</h1>
    <p>State, messages, and the events behind them.</p>
  </div>
  <button
    class="small-button"
    on:click={() => {
      action = action === "spawn" ? "" : "spawn";
      message = "null";
    }}><Plus size={13} /> Spawn actor</button
  >
</div>
{#if error}<div class="error" role="alert">{error}</div>{/if}
{#if selected}<div class="actor-heading">
    <button
      class="text-button"
      on:click={() => {
        selected = null;
        action = "";
      }}><ArrowLeft size={13} /> All actors</button
    >
    <div class="actor-identity">
      <Circle size={15} /><code>{selected.id}</code><span class="badge"
        >{selected.lang === "rust" ? "Rust" : "TS"}</span
      >
    </div>
    <button
      class="cid text-button"
      title={selected.behavior_hash}
      on:click={() => inspect(selected!.behavior_hash)}
      >Behavior {short(selected.behavior_hash, 22)} ↗</button
    >
    <div class="actor-actions">
      <button
        class="small-button"
        on:click={() => {
          action = action === "send" ? "" : "send";
          message = "null";
        }}><Send size={12} /> Send message</button
      ><button
        class="small-button"
        on:click={() => (action = action === "fork" ? "" : "fork")}
        ><GitFork size={12} /> Fork</button
      ><button
        class="small-button"
        on:click={() => (action = action === "upgrade" ? "" : "upgrade")}
        ><ArrowUp size={12} /> Upgrade</button
      >
    </div>
  </div>{/if}
{#if action}<form class="action-form" on:submit|preventDefault={execute}>
    <h2>
      {action === "spawn"
        ? "Spawn an actor"
        : action === "send"
          ? "Send a message"
          : action === "fork"
            ? "Fork this actor"
            : "Upgrade behavior"}
    </h2>
    {#if action === "spawn" || action === "upgrade"}<label
        >Definition<Select label="Definition" bind:value={behavior} placeholder="Choose a definition" options={definitions.filter(def => action === "spawn" || def.lang === selected?.lang).map(def => ({value:def.hash,label:`${definitionName(def)} · ${def.lang}`}))} /></label
      >{/if}{#if action === "send" || action === "spawn"}<label
        >{action === "send" ? "Message" : "Initial state"}<textarea
          bind:value={message}
          aria-label={action === "send"
            ? "Actor message JSON"
            : "Initial actor state JSON"}
          spellcheck="false"
          rows="3"
        ></textarea></label
      >{:else if action === "fork"}<p>
        The new actor starts from this actor’s current state and event history.
      </p>{/if}
    <div>
      <button class="primary" disabled={busy || ((action === "spawn" || action === "upgrade") && !behavior)}
        >{busy
          ? "Working…"
          : action === "spawn"
            ? "Spawn"
            : action === "send"
              ? "Send"
              : action === "fork"
                ? "Fork actor"
                : "Upgrade"}</button
      ><button class="text-button" type="button" on:click={() => (action = "")}
        >Cancel</button
      >
    </div>
  </form>{/if}
{#if actionResult !== undefined}<details class="plain-details">
    <summary>Last action result</summary>
    <div class="detail-body"><ValueView value={actionResult} {inspect} /></div>
  </details>{/if}
{#if selected}<div class="toolbar">
    <div class="tabs">
      <button aria-pressed={tab === "state"} on:click={() => (tab = "state")}
        >State</button
      ><button aria-pressed={tab === "log"} on:click={() => (tab = "log")}
        >Event log <span>{logs.length}</span></button
      >
    </div>
    <span class="quiet">seq {selected.last_seq ?? 0}</span>
  </div>
  {#if busy}<div class="empty">
      Reading actor state…
    </div>{:else if tab === "state"}<section class="actor-state">
      {#if evaluated}<div class="state-caption">LATEST EVALUATION</div>
        <EvaluationView
          value={record(state).result}
          source={typeof record(state).source === "string"
            ? String(record(state).source)
            : ""}
          definition={reference(record(state).def)}
          resultFirst
          {inspect}
        />
        <details class="plain-details">
          <summary>State metadata</summary>
          <div class="detail-body"><ValueView value={state} {inspect} /></div>
        </details>{:else}<div class="detail-body">
          <ValueView value={state} {inspect} />
        </div>{/if}
    </section>{:else}{#each logs as event}<EventCard
        row={eventRow(event)}
        {inspect}
        {loadSource}
      />{:else}<p class="empty">No actor events.</p>{/each}{/if}
{:else}<input class="browser-search" aria-label="Find actors" placeholder="Find actors or behavior…" bind:value={query} /><div class="actor-list">
    {#each actors.filter(actor => (JSON.stringify(actor) + definitionName(definitions.find(def => def.hash === actor.behavior_hash) || {hash:actor.behavior_hash,lang:actor.lang})).toLowerCase().includes(query.toLowerCase())) as actor (actor.id + ":" + actor.last_seq)}<div class="actor-item"><button
        class="actor-row"
        on:click={() => open(actor)}
        ><Circle size={14} />
        <div>
          <code>{short(actor.id, 24)}</code><span
            >{definitionName(
              definitions.find((def) => def.hash === actor.behavior_hash) || {
                hash: actor.behavior_hash,
                lang: actor.lang,
              },
            )}</span
          >
        </div>
        <span class="badge">{actor.lang === "rust" ? "Rust" : "TS"}</span><span
          class="quiet">seq {actor.last_seq ?? 0}</span
        ><ChevronRight size={13} /></button
      ><RowPreview load={async () => resultOf(await client.command("state", {actor:actor.id}))} {inspect} /></div>{:else}<div class="empty">
        No actors yet. Spawn one from a definition.
      </div>{/each}
  </div>{/if}

<style>
  .actor-item :global(.row-preview) { margin:0 38px 16px; }
  .actor-list {
    margin-top: 30px;
    border-top: 1px solid var(--line);
  }
  .actor-row {
    display: flex;
    align-items: center;
    gap: 15px;
    width: 100%;
    padding: 20px 10px;
    border-bottom: 1px solid var(--line);
    text-align: left;
  }
  .actor-row:hover {
    background: var(--tint);
  }
  .actor-row > div {
    flex: 1;
    min-width: 0;
  }
  .actor-row code {
    font-size: 11px;
  }
  .actor-row > div > span {
    display: block;
    color: var(--muted);
    font-size: 10px;
    margin-top: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .actor-heading {
    margin: 28px 0;
  }
  .actor-identity {
    display: flex;
    gap: 10px;
    align-items: center;
    margin: 22px 0 9px;
    overflow-wrap: anywhere;
  }
  .actor-identity code {
    font-size: 12px;
  }
  .actor-actions {
    display: flex;
    gap: 9px;
    margin-top: 20px;
  }
  .actor-heading > .cid {
    font-size: 10px;
  }
  .actor-state {
    background: var(--card);
    border: 1px solid var(--line);
    border-radius: 8px;
    overflow: hidden;
  }
  .state-caption {
    padding: 18px 20px 0;
    font-size: 9px;
    letter-spacing: 0.8px;
    color: var(--muted);
  }
  .actor-state > .plain-details {
    border-top: 1px solid var(--line);
    border-radius: 0;
    border-left: 0;
    border-right: 0;
    border-bottom: 0;
  }
  .action-form {
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--card);
    padding: 20px;
    margin: 20px 0;
  }
  .action-form h2 {
    font-size: 13px;
    margin: 0 0 16px;
  }
  .action-form label {
    display: block;
    font-size: 11px;
    margin: 12px 0;
    color: var(--muted);
  }
  .action-form :global(.select),
  .action-form textarea {
    display: block;
    width: 100%;
    border: 1px solid var(--line);
    border-radius: 5px;
    padding: 10px;
    margin-top: 7px;
    background: var(--code);
    color: var(--ink);
    font-size: 11px;
  }
  .action-form textarea {
    font-family: var(--mono);
  }
  .action-form > div {
    display: flex;
    gap: 15px;
    align-items: center;
  }
  .action-form p {
    font-size: 11px;
    color: var(--muted);
  }
  @media (max-width: 600px) {
    .actor-row {
      gap: 9px;
    }
    .actor-row code {
      font-size: 9px;
    }
    .actor-identity code {
      font-size: 10px;
    }
    .actor-identity {
      flex-wrap: wrap;
    }
    .actor-actions {
      flex-wrap: wrap;
    }
  }
</style>

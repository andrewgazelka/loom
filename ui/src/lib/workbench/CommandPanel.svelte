<script lang="ts">
  import { V, type Command } from "./commands";
  import { onMount, onDestroy } from "svelte";
  import { Play, RefreshCw } from "lucide-svelte";
  import type { WorkbenchClient, ActiveBuild } from "./client";
  import type { Json, Row } from "./schema";
  import DefinitionResult from "./DefinitionResult.svelte";
  import CodeEditor from "../CodeEditor.svelte";
  import type { Journal } from "./journal";
  import type { Workspace } from "./workspace";
  import ActorResult from "./ActorResult.svelte";
  import BuildLog from "./BuildLog.svelte";
  import RunArguments from "./RunArguments.svelte";
  export let journal: Journal;
  export let workspace: Workspace;
  export let replay = false;
  export let command: Command;
  export let client: WorkbenchClient;
  export let defaults: Record<string, string>;
  export let mock: boolean;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
  export let panelId: number;
  const owner = panelId;
  export let completed: (
    owner: number,
    command: Command,
    body: Row,
    result: Json,
  ) => void;
  const session = workspace.open(command, defaults, replay);
  let form: HTMLFormElement;
  let now = Date.now();
  let build: ActiveBuild | null = null;
  let progressError = "";
  let polling = false;
  let disposed = false;
  const progressController = new AbortController();
  async function execute() {
    await session.execute(client, journal, (body, result) =>
      completed(owner, command, body, result),
    );
  }
  async function progress() {
    now = Date.now();
    if (
      disposed ||
      mock ||
      polling ||
      !$session.busy ||
      $session.loadingSource ||
      ![V.add, V.update].some((id) => id === command.id)
    )
      return;
    polling = true;
    try {
      build = await client.activeBuild(progressController.signal);
      progressError = "";
    } catch (error) {
      if (!disposed) progressError = `Build status: ${String(error)}`;
    } finally {
      polling = false;
    }
  }
  onMount(() => {
    const timer = setInterval(() => {
      void progress();
    }, 1000);
    void session.prepare(client).then(() => {
      if (command.read || mock || replay) void execute();
    });
    return () => clearInterval(timer);
  });
  onDestroy(() => {
    disposed = true;
    progressController.abort();
  });
  $: activeBuild =
    build?.name === ($session.values.name?.trim() || "main") ? build : null;
  $: duration =
    $session.startedAt === null ? 0 : Math.max(0, now - $session.startedAt);
</script>

<svelte:window
  on:keydown={(event) => {
    if (
      !event.defaultPrevented &&
      (event.metaKey || event.ctrlKey) &&
      event.key === "Enter" &&
      form?.contains(document.activeElement)
    ) {
      event.preventDefault();
      void execute();
    }
  }}
/>
<section
  class="command-panel"
  aria-label={command.name}
  aria-busy={$session.busy}
>
  <div class="panel-title">
    <div>
      <h1>{command.name}</h1>
      <p>{command.description}</p>
    </div>
    <span class="scope">{command.read ? "Read" : "Execute"}</span>
  </div>
  {#if command.id === V.run}<RunArguments
      {client}
      target={$session.values.target ?? ""}
    />{/if}
  <form
    bind:this={form}
    aria-label={`${command.name} input`}
    on:submit|preventDefault={execute}
  >
    <fieldset disabled={$session.busy}>
      <div class="fields">
        {#each command.fields as field}<div
            class="field"
            class:wide={field.kind === "source" || field.kind === "json"}
          >
            <span
              >{field.label}{#if field.optional || field.default !== undefined}<small
                  >optional</small
                >{/if}</span
            >
            {#if field.options}<select bind:value={$session.values[field.key]}
                >{#each field.options as option}<option value={option}
                    >{option}</option
                  >{/each}</select
              >
            {:else if field.kind === "source" || field.kind === "json"}<CodeEditor
                bind:value={$session.values[field.key]}
                language={field.key === "query"
                  ? V.sql
                  : field.kind === "source"
                    ? "rust"
                    : "json"}
                label={field.label}
                disabled={$session.busy}
                submit={execute}
              />
            {:else}<input
                type="text"
                inputmode={field.kind === "number" ? "numeric" : undefined}
                bind:value={$session.values[field.key]}
                spellcheck="false"
                aria-label={field.label}
              />{/if}
          </div>{/each}
      </div>
      <div class="form-actions">
        {#if $session.dirty}<button
            type="button"
            on:click={() => session.discard(client)}
            disabled={$session.busy}>Discard draft</button
          ><span class="muted">Draft</span>{/if}
        <button class="primary" type="submit" disabled={$session.busy}
          >{#if command.read}<RefreshCw size={12} />{:else}<Play
              size={12}
            />{/if}{$session.busy ? "Running…" : command.name}</button
        >{#if $session.busy}<span role="status" class="muted"
            >{#if $session.loadingSource}Loading stored source…{:else}{activeBuild
                ? {
                    preflight: "Preparing compiler",
                    check: "Checking source",
                    compile: "Compiling Rust",
                    publish: "Publishing definition",
                  }[activeBuild.stage]
                : "Waiting for completion"} · {(duration / 1000).toFixed(0)} s{/if}</span
          >{/if}{#if $session.elapsed !== null}<span class="muted"
            >{mock ? "Fixture response" : "Completed"} · {$session.elapsed} ms</span
          >{/if}
      </div>
    </fieldset>
  </form>
  {#if $session.busy && progressError}<p class="error" role="alert">
      {progressError}
    </p>{/if}
  {#if $session.error}<p class="error" role="alert">{$session.error}</p>{/if}
  {#if $session.result !== undefined}<section
      class="operation-result"
      aria-label={`${command.name} result`}
    >
      {#if command.group === "Definitions"}<DefinitionResult
          operation={command.operation}
          value={$session.result}
          {navigate}
        />
        {#if !mock && [V.add, V.update].some((id) => id === command.id)}<BuildLog
            {client}
            value={$session.result}
          />{/if}
      {:else}<ActorResult
          operation={command.operation}
          value={$session.result}
          actorId={$session.actorId}
          {navigate}
        />{/if}
    </section>{:else if !$session.busy && !$session.error}<p class="note">
      Enter the fields above, then run <code>{command.name}</code>.
    </p>{/if}
</section>

<style>
  .panel-title {
    display: flex;
    align-items: flex-start;
    padding: 14px 16px;
    border-bottom: 1px solid var(--line);
    gap: 12px;
  }
  .panel-title p {
    margin: 4px 0 0;
    color: var(--muted);
  }
  .scope {
    margin-left: auto;
    color: var(--muted);
    font-size: 0.9em;
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 2px 6px;
  }
  form {
    padding: 12px 16px;
    border-bottom: 1px solid var(--line);
    background: var(--side);
  }
  fieldset {
    border: 0;
    margin: 0;
    padding: 0;
    min-width: 0;
  }
  .fields {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px 14px;
  }
  .wide {
    grid-column: 1/-1;
  }
  .field > span {
    display: flex;
    margin-bottom: 5px;
    color: var(--muted);
  }
  small {
    margin-left: 8px;
    font-size: 0.9em;
    opacity: 0.7;
  }
  input,
  select {
    width: 100%;
  }
  .form-actions {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 10px;
  }
  .operation-result {
    min-height: 200px;
  }
</style>

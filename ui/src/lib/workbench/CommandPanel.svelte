<script lang="ts">
  import { V } from "./commands";
  import { onMount, onDestroy } from "svelte";
  import { Play, RefreshCw } from "lucide-svelte";
  import { commandById, parseFields, type Command } from "./commands";
  import { RequestSlot, type WorkbenchClient } from "./client";
  import { definitionView, type Json, type Row } from "./schema";
  import DefinitionResult from "./DefinitionResult.svelte";
  import CodeEditor from "../CodeEditor.svelte";
  import type { Journal } from "./journal";
  export let journal: Journal;
  export let replay = false;
  import ActorResult from "./ActorResult.svelte";
  export let command: Command;
  export let client: WorkbenchClient;
  export let defaults: Record<string, string>;
  export let mock: boolean;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
  export let completed: (body: Row, result: Json) => void;
  let values: Record<string, string> = {};
  for (const field of command.fields)
    values[field.key] = defaults[field.key] ?? field.initial ?? "";
  let result: Json | undefined;
  let form: HTMLFormElement;
  let resultActorId = "";
  let error = "",
    busy = false,
    elapsed: number | null = null;
  const slot = new RequestSlot();
  async function execute() {
    if (busy) return;
    error = "";
    result = undefined;
    elapsed = null;
    const entry = journal.begin(command, values);
    let body;
    try {
      body = parseFields(command, values);
    } catch (problem) {
      error = problem instanceof Error ? problem.message : String(problem);
      journal.fail(entry, error);
      return;
    }
    busy = true;
    const start = performance.now();
    await slot.run(
      async (signal) => {
        try {
          const value = await client.call(command, body, signal);
          journal.finish(entry, value);
          return value;
        } catch (error) {
          journal.fail(entry, String(error));
          throw error;
        }
      },
      (value) => {
        result = value;
        resultActorId = String(body.id ?? "");
        elapsed = Math.round(performance.now() - start);
        completed(body, value);
      },
      (message) => (error = message),
      () => (busy = false),
    );
  }
  onMount(() => {
    if (command.id === V.update && !values.source && values.name) {
      busy = true;
      void slot.run(
        (signal) =>
          client.call(commandById(V.view), { target: values.name! }, signal),
        (result) =>
          (values = { ...values, source: definitionView(result).source }),
        (message) => (error = message),
        () => (busy = false),
      );
      return;
    }
    // Opening a live mutation always requires the operator's explicit Run action.
    if (command.read || mock || replay) {
      try {
        parseFields(command, values);
        void execute();
      } catch {
        /* Incomplete forms await input. */
      }
    }
  });
  onDestroy(() => slot.cancel());
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
<section class="command-panel" aria-label={command.name} aria-busy={busy}>
  <div class="panel-title">
    <div>
      <h1>{command.name}</h1>
      <p>{command.description}</p>
    </div>
    <span class="scope">{command.read ? "Read" : "Execute"}</span>
  </div>
  <form
    bind:this={form}
    aria-label={`${command.name} input`}
    on:submit|preventDefault={execute}
  >
    <fieldset disabled={busy}>
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
            {#if field.options}<select bind:value={values[field.key]}
                >{#each field.options as option}<option value={option}
                    >{option}</option
                  >{/each}</select
              >
            {:else if field.kind === "source" || field.kind === "json"}<CodeEditor
                bind:value={values[field.key]}
                language={field.key === "query"
                  ? V.sql
                  : field.kind === "source"
                    ? "rust"
                    : "json"}
                label={field.label}
                disabled={busy}
                submit={execute}
              />
            {:else}<input
                type="text"
                inputmode={field.kind === "number" ? "numeric" : undefined}
                bind:value={values[field.key]}
                spellcheck="false"
                aria-label={field.label}
              />{/if}
          </div>{/each}
      </div>
      <div class="form-actions">
        <button class="primary" type="submit" disabled={busy}
          >{#if command.read}<RefreshCw size={12} />{:else}<Play
              size={12}
            />{/if}{busy ? "Running…" : command.name}</button
        >{#if elapsed !== null}<span class="muted"
            >{mock ? "Fixture response" : "Completed"} · {elapsed} ms</span
          >{/if}
      </div>
    </fieldset>
  </form>
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if result !== undefined}<section
      class="operation-result"
      aria-label={`${command.name} result`}
    >
      {#if command.group === "Definitions"}<DefinitionResult
          operation={command.operation}
          value={result}
          {navigate}
        />
      {:else}<ActorResult
          operation={command.operation}
          value={result}
          actorId={resultActorId}
          {navigate}
        />{/if}
    </section>{:else if !busy && !error}<p class="note">
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

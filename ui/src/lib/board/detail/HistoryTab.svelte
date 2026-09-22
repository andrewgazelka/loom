<script lang="ts">
  /**
   * The name's hash chain from the `history` verb, newest first. Expanding a revision runs
   * `diff` against the previous hash once and shows what changed.
   */
  import { ChevronDown, ChevronRight } from "lucide-svelte";
  import type { BoardClient } from "../connect";
  import type { Selection } from "../selection";
  import {
    definitionDiff,
    history as parseHistory,
    type DefinitionDiff,
    type Revision,
  } from "../../workbench/schema";
  import { fetched } from "../fetched.svelte";
  import { reason, short, stamp } from "./format";
  let {
    name,
    hash,
    client,
    onselect,
    names,
  }: {
    name: string | null;
    /** The definition on screen, marked "current" in the chain. */
    hash: string;
    client: BoardClient | null;
    onselect: (selection: Selection | null) => void;
    /** Definition hash to name, for chain entries the board knows. */
    names: Record<string, string | null>;
  } = $props();

  const loaded = fetched(
    () => [client, name],
    (owner, target) =>
      owner === null || target === null
        ? null
        : owner
            .command("history", { name: target })
            .then((result): Revision[] => [...parseHistory(result)].reverse()),
  );
  const revisions = $derived(loaded.value);
  const error = $derived(loaded.error);
  // Expanded revisions and their diffs are keyed by revision hash, valid across reconnects.
  let open = $state<Set<string>>(new Set());
  type Loaded = { diff: DefinitionDiff } | { error: string } | { loading: true };
  let diffs = $state<Record<string, Loaded>>({});

  /** The revision before `position` in the chain (older); `null` for the first revision. */
  function previous(position: number): Revision | null {
    return revisions?.[position + 1] ?? null;
  }
  function toggle(revision: Revision, position: number) {
    const next = new Set(open);
    if (next.has(revision.hash)) {
      next.delete(revision.hash);
      open = next;
      return;
    }
    next.add(revision.hash);
    open = next;
    const older = previous(position);
    const owner = client;
    if (older === null || owner === null || diffs[revision.hash]) return;
    diffs = { ...diffs, [revision.hash]: { loading: true } };
    owner.command("diff", { old: older.hash, new: revision.hash }).then(
      (result) => {
        if (client !== owner) return;
        try {
          diffs = { ...diffs, [revision.hash]: { diff: definitionDiff(result) } };
        } catch (problem) {
          diffs = { ...diffs, [revision.hash]: { error: reason(problem) } };
        }
      },
      (problem: unknown) => {
        if (client === owner) diffs = { ...diffs, [revision.hash]: { error: reason(problem) } };
      },
    );
  }
</script>

{#if name === null}
  <div class="note">Unnamed definitions have no history.</div>
{:else if client === null}
  <div class="note">Not connected.</div>
{:else if error !== null}
  <div class="error" role="alert">{error}</div>
{:else if revisions === null}
  <div class="note">Loading history of {name}…</div>
{:else}
  <div class="chain" role="list" aria-label={`Revisions of ${name}, newest first`}>
    {#each revisions as revision, position (revision.hash)}
      {@const older = previous(position)}
      {@const loaded = diffs[revision.hash]}
      <div class="revision" role="listitem" class:current={revision.hash === hash}>
        <div class="row">
          <button
            type="button"
            class="expand"
            aria-expanded={open.has(revision.hash)}
            aria-label={`${open.has(revision.hash) ? "Collapse" : "Expand"} revision ${short(revision.hash)}`}
            onclick={() => toggle(revision, position)}
            >{#if open.has(revision.hash)}<ChevronDown size={12} />{:else}<ChevronRight
                size={12}
              />{/if}</button
          >
          <button
            type="button"
            class="text-control hash"
            title={revision.hash}
            onclick={() => onselect({ kind: "def", hash: revision.hash })}
            >{short(revision.hash)}</button
          >
          {#if revision.hash === hash}<span class="tag">current</span>{/if}
          {#if names[revision.hash] !== undefined && names[revision.hash] !== name}<span
              class="muted">{names[revision.hash]}</span
            >{/if}
          <span class="muted push numeric">{stamp(revision.timestamp)}</span>
        </div>
        {#if open.has(revision.hash)}
          <div class="changes">
            {#if older === null}
              <span class="muted">Initial revision.</span>
            {:else if !loaded || "loading" in loaded}
              <span class="muted">Comparing with {short(older.hash)}…</span>
            {:else if "error" in loaded}
              <span class="error-text">{loaded.error}</span>
            {:else}
              {@const diff = loaded.diff}
              {#if !diff.added.length && !diff.removed.length && !diff.changed.length}
                <span class="muted">No item changed against {short(older.hash)}.</span>
              {/if}
              {#each diff.changed as item (item.name)}<div class="change">
                  <span class="kind">changed</span><span class="item">{item.name}</span><span
                    class="muted">{short(item.old)} → {short(item.new)}</span
                  >
                </div>{/each}
              {#each diff.added as item (item.name)}<div class="change">
                  <span class="kind added">added</span><span class="item">{item.name}</span><span
                    class="muted">{short(item.hash)}</span
                  >
                </div>{/each}
              {#each diff.removed as item (item.name)}<div class="change">
                  <span class="kind removed">removed</span><span class="item">{item.name}</span
                  ><span class="muted">{short(item.hash)}</span>
                </div>{/each}
            {/if}
          </div>
        {/if}
      </div>
    {:else}
      <div class="empty">No revisions returned for {name}.</div>
    {/each}
  </div>
{/if}

<style>
  .revision {
    border-bottom: 1px solid var(--line);
  }
  .revision.current .row {
    background: var(--selection);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    min-height: 30px;
    padding: 2px 12px 2px 6px;
  }
  .expand {
    display: inline-flex;
    padding: 4px;
  }
  .hash {
    font-family: var(--mono);
  }
  .tag {
    border: 1px solid var(--line);
    border-radius: 3px;
    padding: 0 4px;
    font-size: 0.85em;
    color: var(--muted);
  }
  .changes {
    padding: 4px 12px 8px 34px;
    display: grid;
    gap: 3px;
    font-size: 0.95em;
  }
  .change {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  .kind {
    min-width: 60px;
    color: var(--muted);
  }
  .kind.added {
    color: var(--verdict-green);
  }
  .kind.removed {
    color: var(--verdict-red);
  }
  .item {
    font-family: var(--mono);
  }
  .error-text {
    color: var(--error);
  }
</style>

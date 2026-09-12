<script lang="ts">
  import { Check, GitBranch, ScanSearch } from "lucide-svelte";
  import {
    actorTree,
    json,
    object,
    rows,
    validation,
    type ActorNode,
    type Json,
    type Row,
  } from "./schema";
  import DataTable from "./DataTable.svelte";
  import Hash from "./Hash.svelte";
  export let operation: string;
  export let value: Json;
  export let actorId: string;
  export let navigate: (
    command: string,
    overrides?: Record<string, string>,
  ) => void;
  function treeRows(root: ActorNode): Row[] {
    const result: Row[] = [];
    function visit(node: ActorNode, depth: number) {
      result.push({
        depth,
        id: node.id,
        status: node.status,
        cursor: node.cursor,
        behavior_hash: node.behavior_hash,
      });
      node.children.forEach((child) => visit(child, depth + 1));
    }
    visit(root, 0);
    return result;
  }
</script>

{#if operation === "actor_validate"}
  {@const result = validation(value)}
  <div class="verdict" class:matched={result.verdict.kind === "Matched"}>
    <ScanSearch size={21} />
    <div>
      <h2>{result.verdict.kind}</h2>
      <p>
        {result.verdict.kind === "Matched"
          ? "Recorded effects and domain tables match."
          : result.verdict.kind === "DivergedAt"
            ? "The candidate requested a different effect."
            : result.verdict.kind === "Differs"
              ? "Effects match; domain tables differ."
              : "The candidate trapped during replay."}
      </p>
    </div>
  </div>
  {#if result.verdict.kind === "DivergedAt"}
    <div class="section-bar">
      <h3>Effect divergence</h3>
      <span class="numeric"
        >seq {result.verdict.seq} · idx {result.verdict.idx}</span
      >
    </div>
    <DataTable
      label="Divergence details"
      rows={[
        { side: "Expected", bytes: result.verdict.expected },
        { side: "Got", bytes: result.verdict.got },
      ]}
    />
    <p class="note">
      Replay stopped at this effect. This verdict contains no table hashes.
    </p>
  {:else if result.verdict.kind === "Trapped"}<p class="error" role="status">
      Sequence {result.verdict.seq}: {result.verdict.error}
    </p>
  {:else}<div class="section-bar"><h3>Table hashes</h3></div>
    <DataTable
      label="Validation table hashes"
      rows={result.verdict.tables.map((table) => ({ ...table }))}
    />{/if}
  <div class="section-bar">
    <Check size={14} class="icon-run" />
    <h3>Assertions</h3>
    <span class="muted"
      >{result.assertions.filter((item) => item.passed).length}/{result
        .assertions.length} passed</span
    >
  </div>
  <DataTable
    label="SQL assertions"
    rows={result.assertions.map((assertion) => ({ ...assertion }))}
  />
{:else if operation === "actor_info"}
  {@const info = object(value, operation)}
  <div class="section-bar">
    <GitBranch size={15} class="icon-actor" />
    <h2>{actorId}</h2>
    <span class="status" data-status={info.status}>{String(info.status)}</span>
  </div>
  <div class="metrics">
    <div><span>Cursor</span><strong>{String(info.cursor)}</strong></div>
    <div><span>Inbox</span><strong>{String(info.inbox_len)}</strong></div>
    <div>
      <span>Deferred</span><strong
        >{String(info.deferred_len ?? "Not returned")}</strong
      >
    </div>
  </div>
  <dl class="info">
    <dt>Behavior</dt>
    <dd><Hash value={String(info.behavior_hash)} full /></dd>
    <dt>Parent</dt>
    <dd>
      {#if info.parent}<button
          class="text-control"
          on:click={() => navigate("actor_info", { id: String(info.parent) })}
          >{String(info.parent)}</button
        >{:else}Root supervisor{/if}
    </dd>
  </dl>
  <DataTable
    label="Actor details"
    rows={Object.keys(info)
      .filter(
        (key) =>
          ![
            "id",
            "status",
            "cursor",
            "inbox_len",
            "behavior_hash",
            "parent",
          ].includes(key),
      )
      .map((key) => ({ field: key, value: json(info[key], key) }))}
  />
{:else if operation === "actor_tree"}<DataTable
    label="Supervision tree"
    rows={treeRows(actorTree(value))}
  />
{:else if ["actor_list", "actor_lineage", "actor_dead_letters", "actor_sql", "actor_behaviors"].includes(operation)}<DataTable
    label={operation.replace("actor_", "").replaceAll("_", " ")}
    rows={rows(value, operation)}
  />
{:else if Array.isArray(value)}<DataTable
    label={operation}
    rows={value.map((item) => ({ id: item }))}
  />
{:else if value !== null && typeof value === "object"}<DataTable
    label={`${operation} result`}
    rows={[value as Row]}
  />
{:else}<div class="scalar-result">
    {value === null ? "No registered actor found." : String(value)}
  </div>{/if}

<style>
  .verdict {
    color: var(--verdict-red);
    background: color-mix(in srgb, var(--verdict-red) 9%, var(--bg));
    border-left: 3px solid var(--verdict-red);
    display: flex;
    gap: 13px;
    padding: 22px 16px;
    border-bottom: 1px solid var(--line);
  }
  .verdict.matched {
    color: var(--verdict-green);
    background: color-mix(in srgb, var(--verdict-green) 12%, var(--bg));
    border-left-color: var(--verdict-green);
  }
  .verdict h2 {
    font-size: 1.5em;
  }
  .verdict p {
    margin: 5px 0 0;
    color: var(--muted);
  }
  .metrics {
    display: flex;
    border-bottom: 1px solid var(--line);
  }
  .metrics > div {
    min-width: 140px;
    padding: 16px;
    border-right: 1px solid var(--line);
  }
  .metrics span {
    display: block;
    color: var(--muted);
    margin-bottom: 4px;
  }
  .metrics strong {
    font: 1.8em var(--mono);
    font-weight: 400;
  }
  .info {
    padding: 14px 16px;
    margin: 0;
  }
  .scalar-result {
    padding: 20px;
    font-family: var(--mono);
  }
</style>

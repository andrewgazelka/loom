<script lang="ts">
  /** One call trace in focus: outcome, elapsed, entry, definition, args hash, trace hash. Nothing more. */
  import type { Run } from "../feed";
  import { short, stamp } from "../detail/format";
  import type { PaneShared } from "./types";
  let {
    scope,
    run,
    definitionName,
    onselect,
  }: {
    scope: string;
    /** `null` when the feed no longer holds the call's row. */
    run: Run | null;
    definitionName: string | null;
  } & PaneShared = $props();
</script>

<div class="run" data-testid="detail-run" data-scope={scope}>
  {#if run === null}
    <div class="note">
      Run {scope} is not among the events this board holds (the feed keeps the newest 500).
    </div>
  {:else}
    <dl>
      <dt>outcome</dt>
      <dd>
        <span
          class="outcome"
          class:ok={run.outcome === "ok"}
          class:bad={run.outcome !== null && run.outcome !== "ok"}
          >{run.outcome ?? "unknown"}</span
        >{#if !run.completed}<span class="muted"> · checkpoint, still running</span>{/if}
      </dd>
      <dt>elapsed</dt>
      <dd class="numeric">{run.elapsedMs === null ? "—" : `${run.elapsedMs} ms`}</dd>
      <dt>entry</dt>
      <dd class="mono">{run.entry ?? "—"}</dd>
      <dt>definition</dt>
      <dd>
        {#if run.definitionHash !== null}<button
            type="button"
            class="text-control def"
            data-hash={run.definitionHash}
            title={run.definitionHash}
            onclick={() => onselect({ kind: "def", hash: run.definitionHash! })}
            >{definitionName ?? short(run.definitionHash)}</button
          >{:else}—{/if}
      </dd>
      <dt>args hash</dt>
      <dd class="mono muted" title={run.argsHash ?? undefined}>{run.argsHash === null ? "—" : short(run.argsHash)}</dd>
      <dt>trace hash</dt>
      <dd class="mono muted" title={run.traceHash ?? undefined}>{run.traceHash === null ? "—" : short(run.traceHash)}</dd>
      <dt>recorded</dt>
      <dd class="numeric muted">{stamp(run.ts)} · seq {run.seq}</dd>
    </dl>
  {/if}
</div>

<style>
  .run {
    padding: 16px;
  }
  dl {
    max-width: 640px;
  }
  .mono {
    font-family: var(--mono);
  }
  .def {
    font-weight: 500;
  }
  .outcome {
    font: 0.9em var(--mono);
    border: 1px solid var(--line);
    border-radius: 3px;
    padding: 0 5px;
  }
  .outcome.ok {
    color: var(--verdict-green);
    border-color: var(--verdict-green);
  }
  .outcome.bad {
    color: var(--verdict-red);
    border-color: var(--verdict-red);
  }
</style>

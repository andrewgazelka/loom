<script lang="ts">
  import { record } from "./api";
  export let inferred: unknown = undefined;
  export let allowed: unknown = undefined;
  export let observed: unknown = undefined;
  const labels = (value: unknown): string[] => Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  $: analysis = record(inferred);
</script>
<div class="effect-badges">
  {#if inferred !== undefined}<span title="Statically inferred host operations"><span class="label">Inferred</span> {labels(analysis.labels).join(", ") || (analysis.unknown === false ? "none" : "")}{#if analysis.unknown !== false}<span> · unknown dynamic effects</span>{/if}</span>{/if}
  {#if allowed !== undefined}<span title="Host-enforced operation allowlist"><span class="label">Allowed</span> {allowed === null ? "unrestricted" : labels(allowed).join(", ") || "none"}</span>{/if}
  {#if observed !== undefined}<span title="Authorized host operations observed during execution"><span class="label">Observed</span> {labels(observed).join(", ") || "none recorded"}</span>{/if}
</div>
<style>
  .effect-badges {display:flex; flex-wrap:wrap; gap:6px 14px; color:var(--muted); font:9px/1.7 var(--mono);}
  .effect-badges > span {max-width:100%; overflow-wrap:anywhere;}
  .label {color:var(--ink); opacity:.75; margin-right:4px;}
</style>

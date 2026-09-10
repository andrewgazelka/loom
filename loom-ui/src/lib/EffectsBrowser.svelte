<script lang="ts">
  import {onDestroy} from "svelte";
  import { record, casInspection, short, type LogEvent, type Client } from "./api";
  import ProcessResult from "./ProcessResult.svelte";
  import FilesystemChanges from "./FilesystemChanges.svelte";
  import { effectRows } from "./effects";
  import {traceEvents,TraceEffectsReader,type TraceEffectPage,type TraceReadState} from "./trace-effects";
  import { boundedPreview } from "./preview";
  import RowPreview from "./RowPreview.svelte";
  import Select from "./Select.svelte";
  import ValueView from "./ValueView.svelte";
  import ReferenceLink from "./ReferenceLink.svelte";
  export let client: Client;
  export let events: LogEvent[];
  export let inspect: (hash: string) => void;
  export let actor: (id: string) => void;
  let query = "", filter = "all", scope = "";
  let results: Record<string,unknown> = {}, resultErrors: Record<string,string> = {};
  const requested=new Set<string>();
  let traceReads:Record<string,TraceReadState>={};
  let traceReader:TraceEffectsReader|undefined, traceClient:Client|undefined;
  function syncTraces(current:Client,log:LogEvent[]) {
    if(traceClient!==current) {
      traceReader?.dispose();traceReads={};traceClient=current;
      requested.clear();results={};resultErrors={};
      traceReader=new TraceEffectsReader(current,state=>{traceReads={...traceReads,[state.hash]:state};});
    }
    const calls=traceEvents(log),hashes=new Set(calls.map(event=>String(record(event.event).trace_hash)));
    traceReader!.retain(hashes);
    const retained:Record<string,TraceReadState>={};
    for(const hash of hashes)if(traceReads[hash])retained[hash]=traceReads[hash]!;
    traceReads=retained;
    for(const event of calls) {
      const data=record(event.event),hash=String(data.trace_hash);
      if(!traceReads[hash])traceReader!.request(hash,String(data.scope));
    }
  }
  onDestroy(()=>traceReader?.dispose());
  $: syncTraces(client,events);
  $: traces=Object.values(traceReads).reduce<Record<string,TraceEffectPage>>((pages,state)=>{if(state.page)pages[state.hash]=state.page;return pages;},{});
  $: rows=effectRows(events,traces);
  $: loadedCalls=traceEvents(events).map(event=>({event,data:record(event.event),read:traceReads[String(record(event.event).trace_hash)]}));
  $: for(const row of rows) {
    const hash=row.data.result_hash;
    if(row.data.op === "exec" && typeof hash === "string" && !requested.has(hash)) {
      requested.add(hash);
      const owner=client;
      void boundedPreview(async () => casInspection(await owner.command("cas.inspect",{hash}))).then(data=>{if(owner!==client)return;if(data.value === undefined) throw new Error("Result exceeds the structured preview limit; inspect its CAS object for full capture metadata"); results={...results,[hash]:data.value};}).catch(error=>{if(owner===client)resultErrors={...resultErrors,[hash]:String(error)};});
    }
  }
  $: visible = rows.filter(row => {
    const data=row.data;
    const result=record(results[String(data.result_hash)]);
    return (!scope || data.scope === scope) && JSON.stringify(data).toLowerCase().includes(query.toLowerCase()) &&
      (filter === "all" || (filter === "failed" ? row.status === "Failed" : filter === "cancelled" ? row.status === "Cancelled" : filter === "denied" ? data.type === "effect_denied" : Array.isArray(result.filesystem_changes) && result.filesystem_changes.length > 0));
  });
</script>
<div class="section-heading"><div><h1>Effects</h1><p>Host operations and recorded results in the loaded event log.</p></div></div>
<div class="toolbar">
  <Select label="Filter effects" bind:value={filter} options={[{value:"all",label:"All effects"},{value:"files",label:"Filesystem modifications"},{value:"denied",label:"Denied operations"},{value:"failed",label:"Failed operations"},{value:"cancelled",label:"Cancelled operations"}]} />
  <input class="browser-search" aria-label="Find effects" placeholder="Operation, actor, invocation…" bind:value={query} />
</div>
{#if scope}<button class="text-button" on:click={() => scope = ""}>Invocation: {scope} ×</button>{/if}
{#each loadedCalls as call (String(call.data.scope))}
  {#if call.read?.loading}<p class="trace-loading">Reading recorded effects for seq {call.event.seq}…</p>
  {:else if call.read?.error}<p class="effect-error">Effects unavailable for seq {call.event.seq}: {call.read.error} <button class="text-button" on:click={()=>traceReader?.request(String(call.data.trace_hash),String(call.data.scope),call.read?.page?.next_offset??0)}>Retry</button></p>{/if}
  {#if call.read?.page?.next_offset != null}<button class="text-button" disabled={call.read.loading} on:click={()=>traceReader?.request(String(call.data.trace_hash),String(call.data.scope),call.read!.page!.next_offset!)}>Load more effects from seq {call.event.seq}</button>{/if}
{/each}
{#each visible as row (row.id)}
  {@const event=row.event}
  {@const data=row.data}
  <details class="effect-row">
    <summary><span class="operation">{typeof data.op === "string" ? data.op : "Cached result recorded"}</span><span class="status">{row.status}</span><span class="sequence">seq {event.seq}{typeof data.occurrence === "number" ? ` · effect ${data.occurrence}` : ""}</span></summary>
    <div class="effect-detail">
      <div class="links">
        {#if typeof data.actor_id === "string"}<button class="text-button" on:click={() => actor(String(data.actor_id))} title={data.actor_id}>Actor {short(data.actor_id,16)} ↗</button>{/if}
        {#if typeof data.scope === "string"}<button class="text-button" on:click={() => scope = String(data.scope)} title={data.scope}>Invocation ↗</button>{/if}
        {#if typeof data.def_hash === "string"}<ReferenceLink hash={data.def_hash} {inspect} />{/if}
      </div>
      {#if data.op === "exec"}
        {#if resultErrors[String(data.result_hash)]}<p>Capture metadata unavailable: {resultErrors[String(data.result_hash)]}</p>
        {:else if typeof data.result_hash === "string" && !(data.result_hash in results)}<p>Reading capture metadata…</p>
        {:else}<FilesystemChanges value={results[String(data.result_hash)]} {client} {inspect} />{/if}
      {/if}
      {#if data.op === "exec" && results[String(data.result_hash)]}<ProcessResult value={results[String(data.result_hash)]} />
      {:else if data.op !== "exec" && typeof data.result_hash === "string"}<RowPreview {inspect} load={async () => casInspection(await client.command("cas.inspect", {hash:data.result_hash})).value} />{/if}
      {#if data.error}<p class="effect-error">{String(data.error)}</p>{/if}
      <details class="invocation-details"><summary>Details</summary>
        {#if typeof data.desc_hash === "string"}<p class="reference"><span>Descriptor</span><ReferenceLink hash={data.desc_hash} {inspect} /></p><RowPreview {inspect} load={async () => casInspection(await client.command("cas.inspect", {hash:data.desc_hash})).value} />{/if}
        {#if typeof data.result_hash === "string"}<p class="reference"><span>Result</span><ReferenceLink hash={data.result_hash} {inspect} /></p>{/if}
        {#if typeof data.trace_hash === "string"}<p class="reference"><span>Call trace</span><ReferenceLink hash={data.trace_hash} {inspect} /></p>{/if}
        <ValueView value={data} {inspect} />
      </details>
    </div>
  </details>
{:else}<div class="empty">{loadedCalls.some(call=>call.read?.loading) ? "Reading recorded effects…" : filter === "files" ? "No filesystem changes have been captured in the loaded events. Operations such as exec may modify files without recording a before/after snapshot." : "No matching effects in the loaded events."}</div>{/each}
<style>
  .invocation-details {margin-top:16px;font-size:11px;color:var(--muted);}
  .invocation-details > summary {cursor:pointer;}
  .effect-error {font-size:11px;color:var(--error);}
  .effect-row {border-bottom:1px solid var(--line);}
  .effect-row > summary {display:flex; gap:16px; padding:16px 8px; cursor:pointer; align-items:center;}
  .operation {font:12px var(--mono); flex:1;}
  .status,.sequence {font:10px var(--mono); color:var(--muted);}
  .effect-detail {padding:12px 16px 20px; background:var(--code); border-radius:6px;}
  .links {display:flex;flex-wrap:wrap;gap:12px;margin-bottom:12px;}
  .reference {display:flex;gap:14px;font:10px var(--mono);color:var(--muted);}

</style>

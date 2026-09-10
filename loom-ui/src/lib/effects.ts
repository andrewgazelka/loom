import { record, type LogEvent } from "./api";
import {traceEvents,type TraceEffectPage} from "./trace-effects";
export interface EffectRow { id:string; event: LogEvent; data: Record<string, unknown>; status: string; }
export function effectRows(events: LogEvent[], traces:Record<string,TraceEffectPage> = {}): EffectRow[] {
  const pending = new Map<string, EffectRow[]>();
  const orphanRecords = new Map<string, EffectRow[]>();
  const hidden = new Set<EffectRow>();
  const rows: EffectRow[] = [];
  const key = (data: Record<string, unknown>) => JSON.stringify({ scope: data.scope, occurrence: data.occurrence, desc_hash: data.desc_hash });
  for (const event of [...events].sort((left, right) => left.seq - right.seq)) {
    const data = record(event.event), identity = key(data);
    if (data.type === "effect_invoked") {
      const row = { id:`event:${event.seq}`, event, data, status: "Invoked" };
      const queue = pending.get(identity) ?? [];
      queue.push(row);
      pending.set(identity, queue);
      rows.push(row);
    } else if (data.type === "effect_completed") {
      const status = data.error ? "Failed" : data.cached ? "Cached" : "Completed";
      const row = pending.get(identity)?.shift();
      if (row) { row.data = { ...row.data, ...data }; row.status = status; }
      else {
        const recorded = orphanRecords.get(identity)?.shift();
        if (recorded) hidden.add(recorded);
        rows.push({ id:`event:${event.seq}`, event, data, status });
      }
    } else if (data.type === "effect_denied") {
      rows.push({ id:`event:${event.seq}`, event, data, status: "Denied" });
    } else if (data.type === "effect_recorded" && !pending.get(identity)?.length) {
      const row = { id:`event:${event.seq}`, event, data, status: "Recorded" };
      const queue = orphanRecords.get(identity) ?? [];
      queue.push(row);
      orphanRecords.set(identity, queue);
      rows.push(row);
    }
  }
  for(const event of traceEvents(events)) {
    const call=record(event.event),trace=traces[String(call.trace_hash)];
    if(!trace)continue;
    for(const entry of trace.entries) {
      const outcome=entry.outcome;
      rows.push({
        id:JSON.stringify({trace_scope:trace.scope,scope:entry.key.scope,occurrence:entry.key.occurrence}),
        event,
        data:{type:"trace_effect",scope:entry.key.scope,occurrence:entry.key.occurrence,desc_hash:entry.descriptor_hash,
          op:entry.op,def_hash:trace.definition_hash,trace_hash:trace.trace_hash,status:outcome.status,
          ...(outcome.status==='success'?{result_hash:outcome.result_hash}:outcome.status==='error'?{error:outcome.message}:{})},
        status:outcome.status==='success'?'Completed':outcome.status==='error'?'Failed':'Cancelled',
      });
    }
  }
  return rows.filter(row => !hidden.has(row)).sort((left,right)=>left.event.seq-right.event.seq);
}

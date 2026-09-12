import { record, resultOf, type Client, type LogEvent } from "./api";

export type TraceOutcome =
  | { status: "success"; result_hash: string }
  | { status: "error"; message: string }
  | { status: "cancelled" };
export interface TraceEffect {
  key: { scope: string; occurrence: number };
  descriptor_hash: string;
  op: string;
  outcome: TraceOutcome;
}
export interface TraceEffectPage {
  trace_hash: string;
  scope: string;
  definition_hash: string | null;
  entries: TraceEffect[];
  next_offset: number | null;
}
export function traceEvents(events: LogEvent[]): LogEvent[] {
  const latest = new Map<string, LogEvent>();
  for (const event of events) {
    const data = record(event.event);
    if ((data.type === "call_checkpoint" || data.type === "call_completed") && typeof data.scope === "string" && typeof data.trace_hash === "string") {
      const previous = latest.get(data.scope);
      if (!previous || previous.seq < event.seq) latest.set(data.scope, event);
    }
  }
  return [...latest.values()].sort((left, right) => left.seq - right.seq);
}
export function parseTraceEffectPage(value: unknown, hash: string, offset: number): TraceEffectPage {
  const page = record(value);
  if (page.trace_hash !== hash || typeof page.scope !== "string" || !(page.definition_hash === null || typeof page.definition_hash === "string") || !Array.isArray(page.entries)) {
    throw new Error("Invalid trace effect page identity");
  }
  const entries = page.entries.map(value => {
    const entry = record(value), key = record(entry.key), outcome = record(entry.outcome);
    if (typeof key.scope !== "string" || typeof key.occurrence !== "number" || !Number.isSafeInteger(key.occurrence) || key.occurrence < 0 || typeof entry.descriptor_hash !== "string" || typeof entry.op !== "string") {
      throw new Error("Invalid trace effect entry");
    }
    let parsed: TraceOutcome;
    if (outcome.status === "success" && typeof outcome.result_hash === "string") parsed = {status:"success",result_hash:outcome.result_hash};
    else if (outcome.status === "error" && typeof outcome.message === "string") parsed = {status:"error",message:outcome.message};
    else if (outcome.status === "cancelled") parsed = {status:"cancelled"};
    else throw new Error("Invalid trace effect outcome");
    return {key:{scope:key.scope,occurrence:key.occurrence},descriptor_hash:entry.descriptor_hash,op:entry.op,outcome:parsed};
  });
  const next = page.next_offset;
  if (!(next === null || (typeof next === "number" && Number.isSafeInteger(next) && next === offset + entries.length && next > offset))) {
    throw new Error("Invalid trace effect page cursor");
  }
  const keys = new Set(entries.map(entry => JSON.stringify(entry.key)));
  if (keys.size !== entries.length) throw new Error("Duplicate trace effect identity");
  return {trace_hash:hash,scope:page.scope,definition_hash:page.definition_hash,entries,next_offset:next};
}
export async function readTraceEffects(client: Client, hash: string, offset = 0): Promise<TraceEffectPage> {
  return parseTraceEffectPage(resultOf(await client.command("trace.effects", {hash,offset,limit:256})),hash,offset);
}
export function appendTracePage(previous: TraceEffectPage | undefined, next: TraceEffectPage): TraceEffectPage {
  if (!previous) return next;
  if (previous.trace_hash !== next.trace_hash || previous.scope !== next.scope || previous.definition_hash !== next.definition_hash || previous.next_offset !== previous.entries.length) {
    throw new Error("Trace effect pages changed identity");
  }
  const entries = [...previous.entries, ...next.entries];
  if (new Set(entries.map(entry => JSON.stringify(entry.key))).size !== entries.length) throw new Error("Duplicate trace effect across pages");
  return {...next,entries};
}

export interface TraceReadState {
  hash: string;
  scope: string;
  page?: TraceEffectPage;
  error?: string;
  loading: boolean;
}
interface TraceReadRequest {hash:string;scope:string;offset:number}
/** A view owns its reader. Destroying the view stops queued work and ignores
 * in-flight replies without disposing the shared application client. */
export class TraceEffectsReader {
  private disposed = false;
  private running = 0;
  private queue: TraceReadRequest[] = [];
  private states = new Map<string,TraceReadState>();
  constructor(private client:Client,private changed:(state:TraceReadState)=>void) {}
  dispose() {this.disposed=true;this.queue=[];}
  retain(hashes:Set<string>) {
    this.queue=this.queue.filter(request=>hashes.has(request.hash));
    for(const hash of this.states.keys())if(!hashes.has(hash))this.states.delete(hash);
  }
  request(hash:string,scope:string,offset=0) {
    if(this.disposed)return;
    const state=this.states.get(hash);
    if(state?.loading || (offset===0&&state?.page) || (offset!==0&&state?.page?.next_offset!==offset))return;
    this.publish({hash,scope,page:state?.page,loading:true});
    this.queue.push({hash,scope,offset});
    this.pump();
  }
  private publish(state:TraceReadState) {this.states.set(state.hash,state);this.changed(state);}
  private pump() {
    while(!this.disposed&&this.running<4&&this.queue.length) {
      const request=this.queue.shift()!;
      this.running++;
      void this.read(request);
    }
  }
  private async read(request:TraceReadRequest) {
    try {
      const page=await readTraceEffects(this.client,request.hash,request.offset);
      if(this.disposed||!this.states.has(request.hash))return;
      if(page.scope!==request.scope)throw new Error("Trace page does not match the recorded call scope");
      this.publish({hash:request.hash,scope:request.scope,page:appendTracePage(this.states.get(request.hash)?.page,page),loading:false});
    } catch(error) {
      if(!this.disposed&&this.states.has(request.hash))this.publish({hash:request.hash,scope:request.scope,page:this.states.get(request.hash)?.page,error:String(error),loading:false});
    } finally {this.running--;this.pump();}
  }
}

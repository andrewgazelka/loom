import {test,expect} from "bun:test";
import {effectRows} from "../loom-ui/src/lib/effects";
import type {LogEvent} from "../loom-ui/src/lib/api";
const event=(seq:number,type:string,extra:Record<string,unknown>={}):LogEvent=>({seq,actor:"system",ts:0,event:{type,scope:"call/a",occurrence:1,desc_hash:"hash",...extra}});
test("completion joins invocation, preserving pending and denied effects",()=>{
 const rows=effectRows([event(1,"effect_invoked"),event(2,"effect_completed",{cached:true,result_hash:"result"}),event(3,"effect_invoked",{occurrence:2}),event(4,"effect_denied",{occurrence:3})]);
 expect(rows.map(row=>row.status)).toEqual(["Cached","Invoked","Denied"]);
 expect(rows[0]?.data.result_hash).toBe("result");
});
test("completion remains visible when invocation is outside loaded window",()=>{
 expect(effectRows([event(4,"effect_completed",{error:"denied"})])[0]?.status).toBe("Failed");
});

test("page boundary keeps completion without duplicate cache record",()=>{
 const rows=effectRows([event(3,"effect_recorded",{result_hash:"result"}),event(4,"effect_completed",{cached:false,result_hash:"result"})]);
 expect(rows).toHaveLength(1);
 expect(rows[0]?.status).toBe("Completed");
 expect(rows[0]?.event.seq).toBe(4);
});

test("same-key retries retain each attempt's error or cached success",()=>{
 const rows=effectRows([event(1,"effect_invoked"),event(2,"effect_completed",{error:"first failure"}),event(3,"effect_invoked"),event(4,"effect_completed",{cached:true,result_hash:"retry"})]);
 expect(rows.map(row=>row.status)).toEqual(["Failed","Cached"]);
 expect(rows[0]?.data.error).toBe("first failure");
 expect(rows[0]?.data.result_hash).toBeUndefined();
 expect(rows[1]?.data.result_hash).toBe("retry");
});
test("pending retry never steals an earlier completion",()=>{
 const rows=effectRows([event(3,"effect_invoked"),event(2,"effect_completed",{error:"earlier"}),event(1,"effect_invoked")]);
 expect(rows.map(row=>row.status)).toEqual(["Failed","Invoked"]);
 expect(rows[1]?.data.error).toBeUndefined();
});
test("page-cut completion remains separate from subsequent same-key retry",()=>{
 const rows=effectRows([event(2,"effect_completed",{cached:true}),event(3,"effect_invoked")]);
 expect(rows.map(row=>row.status)).toEqual(["Cached","Invoked"]);
});

import {appendTracePage,parseTraceEffectPage,traceEvents,type TraceEffectPage} from "../loom-ui/src/lib/trace-effects";
const tracePage=(hash:string,entries:TraceEffectPage['entries'],next_offset:number|null=null):TraceEffectPage=>({trace_hash:hash,scope:'call/trace',definition_hash:'definition',entries,next_offset});
const traceEntry=(occurrence:number,outcome:TraceEffectPage['entries'][number]['outcome']):TraceEffectPage['entries'][number]=>({key:{scope:'call/trace/spawn:0',occurrence},descriptor_hash:`descriptor${occurrence}`,op:'fs.list',outcome});
const traceEvent=(seq:number,hash:string,type='call_completed'):LogEvent=>event(seq,type,{scope:'call/trace',trace_hash:hash});
test('all effects in one trace remain distinct despite sharing a log sequence',()=>{
 const page=tracePage('trace',[traceEntry(0,{status:'success',result_hash:'result'}),traceEntry(1,{status:'error',message:'permission denied'}),traceEntry(2,{status:'cancelled'})]);
 const rows=effectRows([traceEvent(10,'trace')],{trace:page});
 expect(rows.map(row=>row.status)).toEqual(['Completed','Failed','Cancelled']);
 expect(new Set(rows.map(row=>row.id)).size).toBe(3);
 expect(new Set(rows.map(row=>row.event.seq)).size).toBe(1);
 expect(rows[1]?.data.error).toBe('permission denied');
 expect(rows[2]?.data.result_hash).toBeUndefined();
 expect(rows[0]?.data.def_hash).toBe('definition');
});
test('latest trace supersedes checkpoint while preserving stable occurrence identity',()=>{
 const entry=traceEntry(0,{status:'success',result_hash:'result'});
 const checkpoint=tracePage('checkpoint',[entry]);
 const completed=tracePage('completed',[entry,traceEntry(1,{status:'cancelled'})]);
 const prior=effectRows([traceEvent(9,'checkpoint','call_checkpoint')],{checkpoint});
 const rows=effectRows([traceEvent(10,'completed'),traceEvent(9,'checkpoint','call_checkpoint')],{checkpoint,completed});
 expect(rows).toHaveLength(2);
 expect(rows[0]?.id).toBe(prior[0]?.id);
 expect(rows[0]?.data.trace_hash).toBe('completed');
 expect(traceEvents([traceEvent(10,'completed'),traceEvent(9,'checkpoint','call_checkpoint')])).toHaveLength(1);
});
test('historical and trace effects coexist without merging different attempts',()=>{
 const rows=effectRows([event(1,'effect_invoked'),event(2,'effect_completed',{error:'old failure'}),traceEvent(3,'trace')],{trace:tracePage('trace',[traceEntry(0,{status:'success',result_hash:'new'})])});
 expect(rows.map(row=>row.status)).toEqual(['Failed','Completed']);
 expect(rows[0]?.id).not.toBe(rows[1]?.id);
});
test('trace page validation rejects wrong identity, unknown outcome, malformed cursor and repeated keys',()=>{
 const entry=traceEntry(0,{status:'cancelled'});
 expect(parseTraceEffectPage(tracePage('trace',[entry]),'trace',0).entries).toHaveLength(1);
 expect(()=>parseTraceEffectPage(tracePage('other',[entry]),'trace',0)).toThrow('identity');
 expect(()=>parseTraceEffectPage(tracePage('trace',[entry],0),'trace',0)).toThrow('cursor');
 expect(()=>parseTraceEffectPage(tracePage('trace',[entry,entry]),'trace',0)).toThrow('Duplicate');
 expect(()=>parseTraceEffectPage({...tracePage('trace',[]),entries:[{...entry,outcome:{status:'invented'}}]},'trace',0)).toThrow('outcome');
});
test('trace pagination appends only matching pages with distinct occurrence keys',()=>{
 const first=tracePage('trace',[traceEntry(0,{status:'cancelled'})],1);
 const second=tracePage('trace',[traceEntry(1,{status:'error',message:'failure'})]);
 const combined=appendTracePage(first,second);
 expect(combined.entries).toHaveLength(2);
 expect(combined.next_offset).toBeNull();
 expect(()=>appendTracePage(first,{...second,trace_hash:'other'})).toThrow('identity');
 expect(()=>appendTracePage(first,tracePage('trace',first.entries))).toThrow('Duplicate');
});

import {Client,type Reply} from '../loom-ui/src/lib/api';
import {TraceEffectsReader,type TraceReadState} from '../loom-ui/src/lib/trace-effects';
interface PendingRead {args:Record<string,unknown>;resolve:(value:Reply)=>void}
class ControlledTraceClient extends Client {
 pending:PendingRead[]=[];
 constructor(){super('','');}
 override command(command:string,args:Record<string,unknown>={}):Promise<Reply>{
  expect(command).toBe('trace.effects');
  return new Promise(resolve=>this.pending.push({args,resolve}));
 }
 complete(index:number){const request=this.pending[index]!;request.resolve({ok:true,seq:1,result:tracePage(String(request.args.hash),[])});}
}
test('trace reader bounds requests and stops queued work when view is destroyed',async()=>{
 const client=new ControlledTraceClient(),updates:TraceReadState[]=[];
 const reader=new TraceEffectsReader(client,state=>updates.push(state));
 for(let index=0;index<6;index++)reader.request(`trace${index}`,'call/trace');
 expect(client.pending).toHaveLength(4);
 client.complete(0);await new Promise(resolve=>setTimeout(resolve,0));
 expect(client.pending).toHaveLength(5);
 expect(updates.some(state=>state.hash==='trace0'&&state.page)).toBe(true);
 reader.dispose();const count=updates.length;
 for(let index=1;index<5;index++)client.complete(index);
 await new Promise(resolve=>setTimeout(resolve,0));
 expect(client.pending).toHaveLength(5);
 expect(updates).toHaveLength(count);
});
test('trace reader exposes invalid server scope as an error and permits retry',async()=>{
 const client=new ControlledTraceClient(),updates:TraceReadState[]=[];
 const reader=new TraceEffectsReader(client,state=>updates.push(state));
 reader.request('trace','other-call');client.complete(0);
 await new Promise(resolve=>setTimeout(resolve,0));
 expect(updates.at(-1)?.error).toContain('scope');
 expect(updates.at(-1)?.page).toBeUndefined();
 reader.request('trace','other-call');expect(client.pending).toHaveLength(2);
 reader.dispose();client.complete(1);
});

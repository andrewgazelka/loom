import {readFile} from 'node:fs/promises';
import {object,type LoomResponse} from '../../mcp-client';
export interface Phase4Gate {name:string;pass:boolean;detail:string}
export interface Phase4Client {
  define(name:string,source:string):Promise<string>;
  command(command:string,args:unknown):Promise<LoomResponse>;
  refuse(name:string,source:string):Promise<LoomResponse>;
}
const names=['ordinary pinned-root write and read','preview existing and new files without disk mutation','preview repeated writes collapse and noops disappear','content-addressed handler pin and changed identity','missing content-addressed handler refused','guest-handler root effects replay'];
function assert(value:unknown,message:string):asserts value {if(!value)throw new Error(message);}
export async function phase4(api:Phase4Client):Promise<Phase4Gate[]> {
  const gates:Phase4Gate[]=[];
  async function gate(index:number,body:()=>Promise<void>) {try {await body();gates.push({name:names[index]!,pass:true,detail:'native controls passed'});}catch(error){gates.push({name:names[index]!,pass:false,detail:String(error)});}console.log(JSON.stringify(gates.at(-1)));}
  let machine='',previewHash='';let preview:Record<string,unknown>|undefined;
  const suffix=crypto.randomUUID();const existing=`effects-existing-${suffix}.txt`,newPath=`effects-new-${suffix}.txt`;
  async function previewCall(mode:string) {assert(machine&&previewHash,'preview setup prerequisite failed');return object((await api.command('call',{hash:previewHash,args:[machine,existing,newPath,mode]})).result);}
  await gate(0,async()=>{
    const root=process.env.LOOM_EFFECTS_FIXTURE_ROOT;assert(root,'LOOM_EFFECTS_FIXTURE_ROOT required');
    const record=object((await api.command('machine.create',{root})).result);assert(typeof record.id==='string','machine id missing');machine=record.id;
    previewHash=await api.define('preview',await readFile(new URL('./preview.rs',import.meta.url),'utf8'));
    const result=await previewCall('seed');assert(result.existing==='before'&&result.new===null,JSON.stringify(result));
  });
  await gate(1,async()=>{
    preview=await previewCall('preview');const value=object(preview.preview);const capture=object(value.filesystem_capture);
    assert(capture.preview===true&&value.result==='after',JSON.stringify(preview));
    assert(Array.isArray(preview.decoded),'decoded changes missing');
    const changes=preview.decoded.map(item=>object(item));
    assert(changes.length===2,'expected two changed paths');
    assert(changes.some(change=>change.path===existing&&change.before==='before'&&change.after==='after'),'existing before/after CAS mismatch');
    assert(changes.some(change=>change.path===newPath&&change.before===null&&change.after==='created'),'new file CAS mismatch');
    const disk=await previewCall('disk');assert(disk.existing==='before'&&disk.new===null,`preview mutated disk: ${JSON.stringify(disk)}`);
  });
  await gate(2,async()=>{
    assert(preview,'preview prerequisite failed');const changes=object(preview.preview).filesystem_changes;assert(Array.isArray(changes)&&changes.length===2,'repeated writes did not collapse');
    const noop=await previewCall('noop');assert(JSON.stringify(object(noop.preview).filesystem_changes)==='[]','no-op diff emitted');
    const disk=await previewCall('disk');assert(disk.existing==='before'&&disk.new===null,'no-op preview mutated disk');
  });
  await gate(3,async()=>{
    async function handler(value:number) {return api.define(`stored-${value}`,`#[loom::def(effects=[])] pub fn main()->u64 {0} pub fn handle(_:loom::Op,_:loom::Continuation)->loom::Reply {loom::Reply::Resume(loom::serde_json::json!(${value}))}`);}
    const first=await handler(17),second=await handler(29);assert(first!==second,'changed handler kept same content identity');
    async function caller(hash:string) {return api.define(`stored-caller-${hash}`, `#[loom::def(effects=["sleep"])] pub fn main()->loom::Value {loom::handle_with("${hash}",||loom::abilities::sleep(2000).expect("sleep")).expect("stored handler")}`);}
    const firstCall=await caller(first),secondCall=await caller(second);assert(firstCall!==secondCall,'changed handler pin kept caller identity');
    assert((await api.command('call',{hash:firstCall,args:[]})).result===17,'first exact pin failed');
    assert((await api.command('call',{hash:secondCall,args:[]})).result===29,'second exact pin failed');
    assert((await api.command('call',{hash:firstCall,args:[]})).result===17,'old pin changed after newer handler');
    const upgraded=object((await api.command('upgrade',{old:first,new:second})).result);
    assert(Array.isArray(upgraded.rehashed),'upgrade did not report dependent rewrites');
    const replacement=upgraded.rehashed.map(item=>object(item)).find(item=>item.previous===firstCall);
    assert(replacement,'upgrade omitted existing pinned caller: '+JSON.stringify(upgraded));
    const rewritten=object(replacement.def).hash;
    assert(typeof rewritten==='string'&&rewritten!==firstCall,'upgrade retained old caller identity');
    assert((await api.command('call',{hash:rewritten,args:[]})).result===29,'upgraded handler alias did not execute replacement');
    assert((await api.command('call',{hash:firstCall,args:[]})).result===17,'explicit old content pin mutated after upgrade');
  });
  await gate(4,async()=>{
    const reply=await api.refuse('missing-handler',`#[loom::def(effects=["sleep"])] pub fn main()->u64 {loom::handle_with("${'0'.repeat(64)}",||7).expect("missing")}`);
    assert(!reply.ok&&/dependency definition missing|dependency signature not found|handler definition [0-9a-f]{64} is not stored/.test(JSON.stringify(reply)),JSON.stringify(reply));
  });
  await gate(5,async()=>{
    const hash=await api.define('handler-replay',`#[loom::def(effects=["sleep","now"])] pub fn main()->Vec<loom::Value> {loom::handle_labels(["sleep"],|_,_|loom::Reply::Resume(loom::abilities::now().expect("outer now")),||loom::scope(|scope| {let a=scope.fork(||loom::abilities::sleep(1000).expect("a")).expect("fork");let b=scope.fork(||loom::abilities::sleep(1000).expect("b")).expect("fork");vec![a.join().expect("join"),b.join().expect("join")]}).expect("scope")).expect("handle")}`);
    const before=await api.command('stats',{});const original=await api.command('call',{hash,args:[]});
    const events=(await api.command('events',{after:before.seq,limit:1000})).result;assert(Array.isArray(events),'missing events');
    const event=events.map(item=>object(object(item).event)).find(event=>event.type==='call_completed'&&event.definition_hash===hash);assert(event&&typeof event.scope==='string','missing replay scope');
    const page=object((await api.command('trace.effects',{hash:event.trace_hash,limit:256})).result);assert(Array.isArray(page.entries)&&page.entries.length===2&&page.entries.every(item=>object(item).op==='now'),'callback root effects not recorded');
    for(let attempt=0;attempt<5;attempt++){const replay=await api.command('call.replay',{hash,args:[],scope:event.scope});assert(JSON.stringify(replay.result)===JSON.stringify(original.result),`handler replay mismatch ${attempt}: ${JSON.stringify(replay)}`);}
  });
  return gates;
}

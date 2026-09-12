import {readFile} from 'node:fs/promises';
import {object,type LoomResponse} from '../../mcp-client';
export interface Phase4Gate {name:string;pass:boolean;detail:string}
export interface Phase4Client {
  define(name:string,source:string):Promise<string>;
  command(command:string,args:unknown):Promise<LoomResponse>;
}
const names=['ordinary pinned-root write and read','preview existing and new files without disk mutation','preview repeated writes collapse and noops disappear','guest-handler root effects replay'];
function assert(value:unknown,message:string):asserts value {if(!value)throw new Error(message);}
export async function phase4(api:Phase4Client):Promise<Phase4Gate[]> {
  const gates:Phase4Gate[]=[];
  async function gate(index:number,body:()=>Promise<void>) {try {await body();gates.push({name:names[index]!,pass:true,detail:'native controls passed'});}catch(error){gates.push({name:names[index]!,pass:false,detail:String(error)});}console.log(JSON.stringify(gates.at(-1)));}
  let machine='',previewHash='';let preview:Record<string,unknown>|undefined;
  const suffix=crypto.randomUUID();const existing=`effects-existing-${suffix}.txt`,newPath=`effects-new-${suffix}.txt`;
  async function previewCall(mode:string) {assert(machine&&previewHash,'preview setup prerequisite failed');return object((await api.command('run',{hash:previewHash,args:[machine,existing,newPath,mode]})).result);}
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
    const hash=await api.define('handler-replay',`pub fn main()->Vec<loom::Value> {loom::handle(["sleep"],|_,_|loom::Reply::Resume(loom::now().expect("outer now")),||loom::scope(|scope| {let a=scope.spawn(||loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(1000).expect("encode value")); loom::Value::Object(map) }).expect("a")).expect("spawn child");let b=scope.spawn(||loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(1000).expect("encode value")); loom::Value::Object(map) }).expect("b")).expect("spawn child");Vec::from([a.join().expect("child result"),b.join().expect("child result")])})).expect("handle")}`);
    const before=await api.command('stats',{});const original=await api.command('run',{hash,args:[]});
    const events=(await api.command('events',{after:before.seq,limit:1000})).result;assert(Array.isArray(events),'missing events');
    const event=events.map(item=>object(object(item).event)).find(event=>event.type==='call_completed'&&event.definition_hash===hash);assert(event&&typeof event.scope==='string','missing replay scope');
    const page=object((await api.command('trace.effects',{hash:event.trace_hash,limit:256})).result);assert(Array.isArray(page.entries)&&page.entries.length===2&&page.entries.every(item=>object(item).op==='now'),'callback root effects not recorded');
    for(let attempt=0;attempt<5;attempt++){const replay=await api.command('call.replay',{hash,args:[],scope:event.scope});assert(JSON.stringify(replay.result)===JSON.stringify(original.result),`handler replay mismatch ${attempt}: ${JSON.stringify(replay)}`);}
  });
  return gates;
}

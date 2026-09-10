/** Native compiler/runtime goal. Point only at an isolated daemon/database. */
import {readFile,mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {Database} from 'bun:sqlite';
import {LoomMcpClient, object, type LoomResponse} from '../mcp-client';
import {phase4,type Phase4Gate} from './effects-fixtures/phase4';
interface Gate {name:string;pass:boolean;detail:string}
const names=['fake clock','forward to host','nested shadow, forwarding, pop and total-label refusal','handler outer context','scoped child inheritance and fork-time snapshot','cross-definition call and fork isolation','handler trap frame diagnostic','pure fold handling and root refusal','root-only effect ledger','guest all with deferred continuations','dropped continuation errors without hanging','abandon drains children and preserves limits','residual effect row admission'];
const gates:Gate[]=[];
// A broken transport must not leave this goal alive after per-gate timeouts.
const watchdog=setTimeout(()=>{console.error('effects goal exceeded 20 minutes; incomplete controls fail');process.exit(1);},1_200_000);
let client:LoomMcpClient|undefined;
let phase4Gates:Phase4Gate[]=[];
let controls='';
let timedOut=false;
let hostPerformance:Record<string,unknown>={detail:'not measured'};
let performanceResult:Record<string,unknown>={pass:false,detail:'not measured',target_median_us:20};
function assert(value:unknown,message:string):asserts value {if(!value)throw new Error(message);}
async function bounded<T>(work:Promise<T>,ms:number,label:string):Promise<T> {
  let timer:ReturnType<typeof setTimeout>|undefined;
  try{return await Promise.race([work,new Promise<never>((_,reject)=>{timer=setTimeout(()=>{timedOut=true;reject(new Error(`${label}: timeout after ${ms}ms (not a pass)`));},ms);})]);}
  finally{clearTimeout(timer);}
}
async function raw(command:string,args:unknown):Promise<LoomResponse> {return bounded(client!.callTool('loom_command',{command,args}),120_000,command);}
async function command(command:string,args:unknown) {const reply=await raw(command,args);assert(reply.ok,JSON.stringify(reply));return reply;}
async function define(name:string,source:string) {
  const reply=await bounded(client!.callTool('loom_define',{lang:'rust',name:`effects-gate-${name}`,source}),600_000,`define ${name}`);
  assert(reply.ok,JSON.stringify(reply));const hash=object(object(reply.result).def).hash;assert(typeof hash==='string','missing definition hash');return hash;
}
async function call(mode:string) {return command('call',{hash:controls,args:[mode]});}
async function gate(index:number,body:()=>Promise<void>) {
  try{await body();gates.push({name:names[index]!,pass:true,detail:'native controls passed'});}
  catch(error){gates.push({name:names[index]!,pass:false,detail:String(error)});}
  console.log(JSON.stringify(gates.at(-1)));
}
async function expectError(mode:string,pattern:RegExp) {
  const reply=await bounded(raw('call',{hash:controls,args:[mode]}),5000,mode);
  assert(!reply.ok&&pattern.test(JSON.stringify(reply)),`expected ${pattern}: ${JSON.stringify(reply)}`);
}
async function traceEntries(hash:string,args:unknown[]) {
  const before=await command('stats',{});
  await command('call',{hash,args});
  const events=(await command('events',{after:before.seq,limit:1000})).result;
  assert(Array.isArray(events),'missing events');
  const completed=events.map(item=>object(object(item).event)).filter(event=>event.type==='call_completed'&&event.definition_hash===hash);
  assert(completed.length===1,`expected one completed trace: ${JSON.stringify(completed)}`);
  const page=object((await command('trace.effects',{hash:completed[0]!.trace_hash,limit:256})).result);
  assert(Array.isArray(page.entries)&&page.next_offset===null,'trace page incomplete');return page.entries.map(entry=>object(entry));
}
function actorSource(handled:boolean) {return `#[loom::actor(effects=["sleep"])] pub struct Counter; impl loom::Actor for Counter {type State=i64;type Event=i64;type Msg=i64;fn init()->i64{0}fn handle(_: &i64,msg:i64)->Vec<i64>{vec![msg]}fn fold(state:i64,event:&i64)->i64 {${handled?'loom::handle_labels(["sleep"],|_,_|loom::Reply::Resume(loom::Value::Null),||loom::abilities::sleep(2000)).expect("handle").expect("sleep");':'loom::abilities::sleep(1).expect("root sleep");'}state+event}}`;}
async function nativeControl(mode:'timing'|'cancellation'):Promise<Record<string,unknown>> {
  const executable=process.env.LOOM_HANDLER_BENCH,dbPath=process.env.LOOM_BENCH_DB;
  assert(executable&&dbPath,'LOOM_HANDLER_BENCH and LOOM_BENCH_DB required for actual native full-roundtrip timing');
  const source=await readFile(new URL(`./effects-fixtures/${mode}.rs`,import.meta.url),'utf8');
  const built=await bounded(client!.callTool('loom_define',{lang:'rust',name:`effects-gate-native-${mode}`,source}),600_000,`define native ${mode}`);
  assert(built.ok,JSON.stringify(built));const definition=object(object(built.result).def);
  assert(typeof definition.hash==='string'&&typeof definition.component_hash==='string','native compiled definition artifact identity missing');
  const database=new Database(dbPath,{readonly:true});
  let bytes:Uint8Array;
  try {
    const row=database.query('SELECT d.component_hash,c.bytes FROM defs d JOIN cas c ON c.hash=d.component_hash WHERE d.hash=?').get(definition.hash) as {component_hash:string;bytes:Uint8Array}|null;
    assert(row&&row.component_hash===definition.component_hash&&row.bytes instanceof Uint8Array,'daemon definition and native store artifact disagree');bytes=row.bytes;
  }finally{database.close();}
  const directory=await mkdtemp(join(tmpdir(),'loom-effects-timing-'));
  try {
    const module=join(directory,`${mode}.wasm`);await writeFile(module,bytes);
    const child=Bun.spawn([executable,...(mode==='cancellation'?['--cancel-borrow']:[]),module,definition.component_hash],{stdout:'pipe',stderr:'pipe'});
    const kill=setTimeout(()=>child.kill(),120_000);
    let output:string,diagnostics:string,code:number;
    try {const result=await Promise.all([new Response(child.stdout).text(),new Response(child.stderr).text(),child.exited]);output=result[0];diagnostics=result[1];code=result[2];}finally{clearTimeout(kill);}
    let result:Record<string,unknown>;
    try {result=object(JSON.parse(output.trim()),'native timing result');}catch(error){throw new Error(`native timing exit ${code}; ${String(error)}\nstdout: ${output}\nstderr: ${diagnostics}`);}
    assert(result.module_hash===definition.component_hash,'native module identity mismatch: '+output);
    assert(typeof result.scope==='string'&&result.scope.startsWith(mode==='cancellation'?'handler-cancellation:':'handler-timing:')&&typeof result.engine_executable_hash==='string'&&/^[0-9a-f]{64}$/.test(result.engine_executable_hash),'native timing scope/executable identity missing');
    if(mode==='cancellation') {
      assert(code===0&&result.pass===true&&result.active_before_cancel===true&&result.drained===true&&result.parent_storage_reused_after_drain===true&&result.stores_after_drain===0&&typeof result.handler_instance_reuses==='number'&&result.handler_instance_reuses>=1,`borrowed callback cancellation failed; exit ${code}\n${output}\n${diagnostics}`);
      return {...result,native_exit:code,diagnostics,definition_hash:definition.hash,compiler_artifact_verified:true};
    }
    assert(result.samples===10000&&result.unit==='us','native timing sample count mismatch: '+output);
    assert(typeof result.median==='number'&&Number.isFinite(result.median)&&result.median>=0&&typeof result.p99==='number'&&Number.isFinite(result.p99)&&result.p99>=result.median,'invalid native timing percentiles');
    assert(result.measurement==='complete guest export upper bound: handler installation, typed perform codec, handler instance dispatch, resume and handler removal','unexpected timing boundary');
    assert(result.target_median_us===20&&typeof result.pass==='boolean','invalid native timing verdict');
    return {...result,pass:code===0&&result.pass===true&&result.median<20,native_exit:code,diagnostics,definition_hash:definition.hash,compiler_artifact_verified:true};
  }finally{await rm(directory,{recursive:true,force:true});}
}
try {
  const endpoint=process.env.LOOM_URL,tokenFile=process.env.LOOM_TOKEN_FILE;
  assert(endpoint&&tokenFile,'Set isolated LOOM_URL and LOOM_TOKEN_FILE');
  client=new LoomMcpClient({endpoint,token:(await readFile(tokenFile,'utf8')).trim()});
  await bounded(client.connect(),10_000,'connect');
  controls=await define('controls',await readFile(new URL('./effects-fixtures/controls.rs',import.meta.url),'utf8'));
  await gate(0,async()=>{const start=performance.now();const reply=await call('fake');assert(reply.result===73,JSON.stringify(reply));assert(performance.now()-start<1000,'fake 2000ms sleep did not return promptly');});
  await gate(1,async()=>{const start=Date.now();const result=(await call('forward')).result;assert(typeof result==='number'&&result>=start-1000&&result<=Date.now()+1000,`not a real host timestamp: ${JSON.stringify(result)}`);});
  await gate(2,async()=>{assert(JSON.stringify((await call('nested')).result)==='[22,11,11]','nested shadow/forward/pop restoration mismatch');await expectError('total-forward',/selected handler label cannot forward/);});
  await gate(3,async()=>{
    const root=process.env.LOOM_EFFECTS_FIXTURE_ROOT;assert(root,'Set LOOM_EFFECTS_FIXTURE_ROOT to server-side effects-fixtures directory');
    const machine=object((await command('machine.create',{root})).result);assert(typeof machine.id==='string','machine id missing');
    const hash=await define('outer',`#[loom::def(effects=["sleep","fs.read"])] pub fn main(machine:String)->loom::Value {let mut depth=0_u32;let text=loom::handle(|_,_|{depth+=1;loom::Reply::Resume(loom::serde_json::json!(loom::abilities::fs::read(&machine,"outer-context.txt").expect("outer read")))},||loom::abilities::sleep(2000).expect("perform")).expect("handle");loom::serde_json::json!({"text":text,"depth":depth})}`);
    const result=object((await command('call',{hash,args:[machine.id]})).result);assert(result.depth===1&&result.text==='outer handler fixture\n',JSON.stringify(result));
  });
  await gate(4,async()=>{assert((await call('inherit')).result===91,'child did not inherit handler');assert(JSON.stringify((await call('snapshot')).result)==='[31,47]','children did not inherit fork-time handler snapshots');});
  await gate(5,async()=>{
    const child=await define('cross-child','#[loom::def(effects=["sleep"])] pub fn main(ms:u64)->loom::Value {loom::abilities::sleep(ms).expect("real child sleep")}');
    const parent=await define('cross-parent',`#[loom::def(effects=["fork","join","call","sleep"])] pub fn main(direct:bool)->Vec<loom::Value>{loom::handle_labels(["sleep"],|_,_|loom::Reply::Resume(loom::serde_json::json!(99)),||{if direct {vec![loom::call(loom::Def::<fn(u64)->loom::Value>::new("${child}"),200).expect("call")]} else {let child=loom::fork(loom::Def::<fn(u64)->loom::Value>::new("${child}"),200).expect("fork");loom::join(vec![child]).expect("join")}}).expect("handle")}`);
    for(const direct of [false,true]) {const start=performance.now();const reply=await command('call',{hash:parent,args:[direct]});assert(JSON.stringify(reply.result)==='[null]'&&performance.now()-start>=180,`cross-definition ${direct?'call':'fork'} inherited handler: ${JSON.stringify(reply)}`);}
  });
  await gate(6,async()=>{
    const before=await command('stats',{});
    const reply=await bounded(raw('call',{hash:controls,args:['trap']}),5000,'handler trap');
    const diagnostic=JSON.stringify(reply);
    assert(!reply.ok&&/handler frame \d+/i.test(diagnostic)&&/unreachable|trap/i.test(diagnostic),'expected handler-frame trap: '+diagnostic);
    const events=(await command('events',{after:before.seq,limit:1000})).result;
    assert(Array.isArray(events),'missing failed-call events');
    const completed=events.map(item=>object(object(item).event)).filter(event=>event.type==='call_completed'&&event.definition_hash===controls);
    assert(completed.length===1,'missing exact failed handler trace: '+JSON.stringify(completed));
    const page=object((await command('trace.effects',{hash:completed[0]!.trace_hash,limit:256})).result);
    assert(Array.isArray(page.entries)&&page.next_offset===null,'incomplete callback-entry trace');
    assert(page.entries.length===1&&object(page.entries[0]).op==='now'&&object(object(page.entries[0]).outcome).status==='success','handler did not reach callback before trapping: '+JSON.stringify(page));
  });
  await gate(7,async()=>{
    for(const handled of [true,false]) {const hash=await define(`fold-${handled}`,actorSource(handled));const actor=object((await command('spawn',{hash,initial:0})).result).id;assert(typeof actor==='string','actor id missing');const reply=await raw('send',{actor,msg:7});assert(handled?reply.ok&&reply.result===7:!reply.ok&&/effects forbidden in pure core execution|fold cannot perform effects/.test(JSON.stringify(reply)),JSON.stringify(reply));}
  });
  await gate(8,async()=>{
    const fake=await traceEntries(controls,['fake']);assert(fake.length===0,`guest effect recorded: ${JSON.stringify(fake)}`);
    const root=await traceEntries(controls,['forward']);assert(root.length===1&&root[0]!.op==='now',`positive ledger control: ${JSON.stringify(root)}`);
    const again=await traceEntries(controls,['fake']);assert(again.length===0,'second guest-handled call recorded effects');
  });
  await gate(9,async()=>{const result=object((await call('deferred')).result);assert(JSON.stringify(result.actual)==='[null,null]'&&JSON.stringify(result.actual)===JSON.stringify(result.expected),JSON.stringify(result));});
  await gate(10,async()=>{assert((await call('catch-drop')).result==='continuation dropped','dropped continuation did not return catchable exact error');await expectError('drop',/continuation dropped/);});
  await gate(11,async()=>{
    await expectError('abandon',/abandon|cancel/i);
    const cancellation=await nativeControl('cancellation');
    console.log(JSON.stringify({stage:'borrowed-handler-cancellation',...cancellation}));
    assert((await call('fake')).result===73,'fresh execution after abandon failed');
    const child=Bun.spawn(['bun',new URL('./shared-limits.ts',import.meta.url).pathname],{env:process.env,stdout:'pipe',stderr:'pipe'});
    const timer=setTimeout(()=>child.kill(),600_000);
    try {const result=await Promise.all([new Response(child.stdout).text(),new Response(child.stderr).text(),child.exited]);const output=result[0];const diagnostics=result[1];const code=result[2];assert(code===0&&/^6\/6 shared limit controls pass$/m.test(output),`shared limits exit ${code}\n${output}\n${diagnostics}`);}finally{clearTimeout(timer);}
  });
  await gate(12,async()=>{
    const body='loom::abilities::fs::read("local","fixture").expect("read")';
    const denied=await bounded(client!.callTool('loom_define',{lang:'rust',name:'effects-gate-row-denied',source:`#[loom::def(effects=["sleep"])] pub fn main()->String {loom::abilities::sleep(0).expect("sleep");${body}}`}),600_000,'define residual refusal');
    assert(!denied.ok&&/residual/i.test(JSON.stringify(denied))&&/fs\.read/.test(JSON.stringify(denied)),JSON.stringify(denied));
    const hash=await define('row-handled',`#[loom::def(effects=["sleep"])] pub fn main()->String {loom::abilities::sleep(0).expect("sleep");loom::handle_labels(["fs.read"],|_,_|loom::Reply::Resume(loom::serde_json::json!("fixture")),||${body}).expect("handle")}`);
    assert((await command('call',{hash,args:[]})).result==='fixture','handled residual did not run');
  });
  phase4Gates=await phase4({define,command,refuse:(name,source)=>bounded(client!.callTool('loom_define',{lang:'rust',name:`effects-gate-${name}`,source}),600_000,`define ${name}`)});
  try {
    const reply=await call('perf');assert(reply.result===10000,'performance workload incomplete');
    const stats=object((await command('stats',{})).result);
    const timing=object(stats.handler_round_trip_us,'handler timing');
    assert(timing.samples===10000&&typeof timing.median==='number'&&typeof timing.p99==='number','missing exact 10000 native handler samples');
    hostPerformance={...timing,measurement:"host dispatch through handler response; guest codec excluded",pass:timing.median<20,target_median_us:20};
  }catch(error){hostPerformance={detail:String(error)};}
  try {performanceResult=await nativeControl('timing');}catch(error){performanceResult={pass:false,detail:String(error),target_median_us:20};}
} catch(error) {for(const name of names)if(!gates.some(g=>g.name===name))gates.push({name,pass:false,detail:String(error)});}
finally {
  try{await bounded(client?.close()??Promise.resolve(),5000,'close');}catch(error){console.error(String(error));}
  clearTimeout(watchdog);
  const passed=gates.filter(g=>g.pass).length;
  console.log(JSON.stringify({stage:'effects-handlers',passed,total:13,first_failure:gates.find(g=>!g.pass)?.name??null,gates,phase4:{passed:phase4Gates.filter(g=>g.pass).length,total:6,gates:phase4Gates},performance:performanceResult,host_dispatch:hostPerformance}));
  console.log(`${passed}/13 effects handler checks pass`);
  console.log(`${phase4Gates.filter(g=>g.pass).length}/6 phase4 checks pass`);
  console.log(`first failing step: ${gates.find(g=>!g.pass)?.name??'none'}`);
  console.log(`handler round trip: ${JSON.stringify(performanceResult)}`);
  // Functional and performance verdicts stay separate, but both are required for exit zero.
  if(passed!==13||phase4Gates.filter(g=>g.pass).length!==6||performanceResult.pass!==true)process.exitCode=1;
  if(timedOut)process.exit(1);
}

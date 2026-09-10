/** Real compiler/runtime admission gate. Use an isolated daemon and database. */
import {readFile} from 'node:fs/promises';
import {LoomMcpClient, object} from '../mcp-client';

interface Gate {name:string; pass:boolean; detail:string}
const names = [
  'borrowed captures and shared atomics',
  'effects from scoped children',
  'separate execution memories',
  'unsafe guest code rejected',
  'non-Send captures rejected',
  'escaping child references rejected',
  'scoped effects replay',
];
const gates:Gate[] = [];
let client:LoomMcpClient|undefined;
function assert(value:unknown, message:string):asserts value {if(!value)throw new Error(message);}
async function define(name:string, source:string) {
  const reply=await client!.callTool('loom_define',{lang:'rust',name:`shared-gate-${name}`,source});
  assert(reply.ok,JSON.stringify(reply));
  const def=object(object(reply.result).def);
  assert(typeof def.hash==='string','missing definition hash');
  return def.hash;
}
async function command(command:string,args:unknown) {
  const reply=await client!.callTool('loom_command',{command,args});
  assert(reply.ok,JSON.stringify(reply));
  return reply;
}
async function gate(name:string, run:()=>Promise<void>) {
  try {await run(); gates.push({name,pass:true,detail:'native compiler/runtime control passed'});}
  catch(error) {gates.push({name,pass:false,detail:String(error)});}
}
const captures=`
use std::sync::atomic::{AtomicU32,Ordering};
static COUNT:AtomicU32=AtomicU32::new(0);
#[loom::def(effects=[])]
pub fn main()->Vec<u32> {
    let values=vec![1_u32,2,3,4];
    loom::scope(|s| {
        let sum=s.fork(|| {
            COUNT.fetch_add(1,Ordering::SeqCst);
            while COUNT.load(Ordering::SeqCst)<2 {std::hint::spin_loop();}
            values.iter().sum::<u32>()
        }).expect("fork");
        let product=s.fork(|| {COUNT.fetch_add(1,Ordering::SeqCst); values.iter().product::<u32>()}).expect("fork");
        vec![sum.join().expect("join"),product.join().expect("join"),COUNT.load(Ordering::SeqCst)]
    }).expect("scope")
}`;
const effects=`
#[loom::def(effects=["sleep"])]
pub fn main()->Vec<String> {
    let label=String::from("borrowed");
    loom::scope(|s| {
        let a=s.fork(|| {loom::abilities::sleep(1).expect("sleep"); label.clone()}).expect("fork");
        let b=s.fork(|| {loom::abilities::sleep(2).expect("sleep"); label.clone()}).expect("fork");
        vec![a.join().expect("join"),b.join().expect("join")]
    }).expect("scope")
}`;
let capturesHash='';
let effectsHash='';
let effectScope='';
try {
  const endpoint=process.env.LOOM_URL, tokenFile=process.env.LOOM_TOKEN_FILE;
  assert(endpoint&&tokenFile,'Set LOOM_URL and LOOM_TOKEN_FILE for an isolated daemon');
  client=new LoomMcpClient({endpoint,token:(await readFile(tokenFile,'utf8')).trim()});
  await client.connect();
  await gate(names[0]!,async()=>{
    capturesHash=await define('captures',captures);
    const reply=await command('call',{hash:capturesHash,args:[]});
    assert(JSON.stringify(reply.result)==='[10,24,2]','shared capture/atomic result mismatch');
  });
  assert(gates[0]?.pass,'positive shared-memory control failed; dependent controls remain unproven');
  await gate(names[1]!,async()=>{
    effectsHash=await define('effects',effects);
    const before=await command('stats',{});
    const reply=await command('call',{hash:effectsHash,args:[]});
    assert(JSON.stringify(reply.result)==='["borrowed","borrowed"]','scoped effects mismatch');
    const events=(await command('events',{after:before.seq,limit:1000})).result;
    assert(Array.isArray(events),'missing event list');
    const completed=events.map(value=>object(object(value).event)).find(event=>event.type==='call_completed'&&event.definition_hash===effectsHash);
    assert(completed&&typeof completed.scope==='string','missing scoped call trace');
    effectScope=completed.scope;
  });
  await gate(names[2]!,async()=>{
    const replies=await Promise.all([
      command('call',{hash:capturesHash,args:[]}),
      command('call',{hash:capturesHash,args:[]}),
    ]);
    for(const reply of replies) {
      assert(JSON.stringify(reply.result)==='[10,24,2]','static memory leaked across executions');
    }
  });
  const refusals=[
    {name:names[3]!,source:'#[loom::def(effects=[])] pub fn main()->u32 { unsafe { std::ptr::read_volatile(&1) } }',reason:/unsafe/i},
    {name:names[4]!,source:'#[loom::def(effects=[])] pub fn main()->u32 { let value=std::rc::Rc::new(1); loom::scope(|s| s.fork(move || *value).expect("fork").join().expect("join")).expect("scope") }',reason:/Send|sent between threads/},
    {name:names[5]!,source:'#[loom::def(effects=[])] pub fn main()->String { loom::scope(|s| { let job=s.fork(|| { let value=String::from("local"); value.as_str() }).expect("fork"); job.join().expect("join").to_owned() }).expect("scope") }',reason:/E0515|cannot return.*(?:local|owned)|borrowed|does not live long enough/},
  ];
  for(const control of refusals) await gate(control.name,async()=>{
    const reply=await client!.callTool('loom_define',{lang:'rust',name:'shared-gate-refusal',source:control.source});
    assert(!reply.ok&&control.reason.test(JSON.stringify(reply)),'expected specific compiler refusal');
  });
  await gate(names[6]!,async()=>{
    assert(effectScope,'effect trace prerequisite failed');
    const reply=await command('call.replay',{hash:effectsHash,args:[],scope:effectScope});
    assert(JSON.stringify(reply.result)==='["borrowed","borrowed"]','scoped replay mismatch');
  });
} catch(error) {
  for(const name of names) if(!gates.some(gate=>gate.name===name)) gates.push({name,pass:false,detail:String(error)});
} finally {
  await client?.close();
  const passed=gates.filter(gate=>gate.pass).length;
  console.log(JSON.stringify({stage:'shared-execution-goal',passed,total:names.length,first_failure:gates.find(gate=>!gate.pass)?.name??null,gates}));
  console.log(`${passed}/${names.length} shared-execution gates pass`);
  if(passed!==names.length)process.exitCode=1;
}

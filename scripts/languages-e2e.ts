/** M6: real cross-language components through the authenticated production API. */
import {readFile} from 'node:fs/promises';
const endpoint=process.env.LOOM_URL??'http://127.0.0.1:8787';
const token=process.env.LOOM_TOKEN;
if(!token)throw new Error('LOOM_TOKEN is required');
interface Diagnostic {code:string;message:string}
interface Reply {ok:boolean;result:unknown;diagnostics:Diagnostic[]}
interface Definition {def:{hash:string};build:{ms:number;size:number}}
interface SpawnedActor {id:string}
let passed=0;
const total=3;
function assert(condition:unknown,message:string):asserts condition {if(!condition)throw new Error(message);}
async function operation<T>(name:string,body:unknown):Promise<T>{
  const response=await fetch(`${endpoint}/v1/${name}`,{method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`},body:JSON.stringify(body)});
  if(!response.ok)throw new Error(`${name}: HTTP ${response.status}: ${await response.text()}`);
  const reply=await response.json() as Reply;
  assert(reply.ok,JSON.stringify(reply));return reply.result as T;
}
async function define(name:string,lang:'ts'|'rust',source:string,deps:Record<string,string>={}):Promise<Definition>{return operation('define',{name,lang,source,deps});}
async function call<T>(hash:string,args:unknown):Promise<T>{return operation('command',{command:'call',args:{hash,args}});}
async function spawn(hash:string):Promise<SpawnedActor>{return operation('command',{command:'spawn',args:{hash,initial:0}});}
async function send(actor:string,msg:unknown):Promise<unknown>{return operation('command',{command:'send',args:{actor,msg}});}
async function state(actor:string):Promise<number>{return operation('command',{command:'state',args:{actor}});}
async function statesAfterDelivery(tsActor:string,rustActor:string,expected:number):Promise<void>{
  const deadline=performance.now()+10000;
  let observed={ts:await state(tsActor),rust:await state(rustActor)};
  while(observed.ts!==expected||observed.rust!==expected){
    if(performance.now()>=deadline){
      const failures=await operation('command',{command:'events',args:{actor:'system',limit:20}});
      throw new Error(JSON.stringify({message:'actor delivery/fold deadline',expected,observed,tsActor,rustActor,failures}));
    }
    await Bun.sleep(20);
    observed={ts:await state(tsActor),rust:await state(rustActor)};
  }
}
async function check(name:string,test:()=>Promise<void>){await test();passed++;console.log(`PASS ${name}`);}
try {
  await check('TS and Rust actors exchange messages in both directions',async()=>{
    const tsSource='import {actor} from "loom";type Message=number|{actor:string;value:number};export function run(_state:number,msg:Message):number[]{if(typeof msg==="number")return [msg];actor.send(msg.actor,msg.value);return [msg.value];}export function fold(state:number,event:number):number{return state+event;}';
    const rustSource=`#[loom::actor(effects=["actor.send"])]
pub struct Messenger;
impl loom::Actor for Messenger {
 type State=i64;type Event=i64;type Msg=serde_json::Value;
 fn init()->i64{0}
 fn fold(state:i64,event:&i64)->i64{state+event}
 fn handle(_state:&i64,message:serde_json::Value)->Vec<i64>{
  if let Some(value)=message.as_i64(){return vec![value];}
  let actor=message["actor"].as_str().expect("actor");
  let value=message["value"].as_i64().expect("value");
  loom::actor::send(actor,serde_json::json!(value)).expect("actor.send");
  vec![value]
 }
}`;
    const ts=await spawn((await define('m6-ts-messenger','ts',tsSource)).def.hash);
    const rust=await spawn((await define('m6-rust-messenger','rust',rustSource)).def.hash);
    await send(ts.id,{actor:rust.id,value:7});
    await statesAfterDelivery(ts.id,rust.id,7);
    await send(rust.id,{actor:ts.id,value:11});
    await statesAfterDelivery(ts.id,rust.id,18);
  });
  await check('Rust recurses through synchronous definition calls',async()=>{
    const definition=await define('m6-recursive','rust',await readFile(new URL('../examples/rust-recursive/src/lib.rs',import.meta.url),'utf8'));
    assert(await call<number>(definition.def.hash,[4])===4,'recursive definition call failed');
  });
  await check('new Rust source builds in under five seconds with warm dependencies',async()=>{
    const nonce=Date.now();
    await define('m6-warm-baseline','rust',`#[loom::def(effects=[])]pub fn warm()->i64{${nonce}}`);
    const measured=await define('m6-warm-measured','rust',`#[loom::def(effects=[])]pub fn warm()->i64{${nonce+1}}`);
    assert(measured.build.size>0,'no component artifact');
    assert(measured.build.ms<5000,`warm build took ${measured.build.ms} ms`);
    assert(await call<number>(measured.def.hash,[])===nonce+1,'measured component did not consume new source');
    console.log(`warm Rust build ${measured.build.ms} ms`);
  });
} finally {console.log(`${passed}/${total} M6 native language checks pass`);}

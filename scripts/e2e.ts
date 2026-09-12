/** Native HTTP path: the daemon must build and execute real guest core wasm modules. */
import { readFile } from 'node:fs/promises';
const endpoint = process.env.LOOM_URL ?? 'http://127.0.0.1:8787';
const token = process.env.LOOM_TOKEN;
if (!token) throw new Error('Set LOOM_TOKEN to the running daemon token');
type Reply = {ok:boolean;seq:number;result:any;diagnostics:any[]};
let passed = 0;
async function request(operation:string,body:unknown):Promise<Reply> {
  const response=await fetch(`${endpoint}/v1/${operation}`,{method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`},body:JSON.stringify(body)});
  if (!response.ok) throw new Error(`${operation}: HTTP ${response.status}: ${await response.text()}`);
  return response.json() as Promise<Reply>;
}
function assert(condition:unknown,message:string):asserts condition {if(!condition) throw new Error(message);}
async function accepted(operation:string,body:unknown):Promise<Reply> {const reply=await request(operation,body);assert(reply.ok,JSON.stringify(reply));return reply;}
async function check(name:string,test:()=>Promise<void>) {await test();passed++;console.log(`PASS ${name}`);}
try {
  await check('HTTP authentication',async()=>{const response=await fetch(`${endpoint}/v1/events`);assert(response.status===401,'unauthenticated read accepted');});
  let rustHash='';
  await check('Rust build and Wasmtime execution',async()=>{
    const rejected=await request('define',{lang:'rust',name:'e2e-rust-rejected',source:'#[loom::def(effects=[])] pub fn main() -> i64 { "wrong" }'});
    assert(!rejected.ok && rejected.diagnostics.some(diagnostic=>diagnostic.code==='E0308'),'Rust type error lacked structured E0308');
    const source=await readFile(new URL('../examples/rust-add/src/lib.rs',import.meta.url),'utf8');
    const reply=await accepted('define',{lang:'rust',name:'e2e-rust-add',source});rustHash=reply.result.def.hash;
    assert(reply.result.build.size>0,'empty wasm module');
    const result=await accepted('command',{command:'call',args:{hash:rustHash,args:[20,22]}});assert(result.result===42,JSON.stringify(result));
  });
  await check('Rust actor event fold and fork',async()=>{
    const source=await readFile(new URL('../examples/rust-counter/src/lib.rs',import.meta.url),'utf8');
    const definition=await accepted('define',{lang:'rust',name:'e2e-counter',source});
    const actor=await accepted('command',{command:'spawn',args:{hash:definition.result.def.hash,initial:0}});
    await accepted('command',{command:'send',args:{actor:actor.result.id,msg:7}});
    const state=await accepted('command',{command:'state',args:{actor:actor.result.id}});assert(state.result===7,JSON.stringify(state));
    const fork=await accepted('command',{command:'fork',args:{actor:actor.result.id}});
    await accepted('command',{command:'send',args:{actor:fork.result.id,msg:3}});
    assert((await accepted('command',{command:'state',args:{actor:fork.result.id}})).result===10,'fork failed');
    assert((await accepted('command',{command:'state',args:{actor:actor.result.id}})).result===7,'fork changed parent');
  });
  await check('interactive Rust eval',async()=>{const reply=await accepted('eval',{source:'6 * 7'});assert(reply.result.value===42,JSON.stringify(reply));});
} finally {console.log(`${passed}/4 native HTTP checks pass`);}

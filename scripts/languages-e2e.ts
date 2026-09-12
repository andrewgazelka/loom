/** M6: real Rust guests through the authenticated production API. */
import {readFile} from 'node:fs/promises';
const endpoint=process.env.LOOM_URL??'http://127.0.0.1:8787';
const token=process.env.LOOM_TOKEN;
if(!token)throw new Error('LOOM_TOKEN is required');
interface Diagnostic {code:string;message:string}
interface Reply {ok:boolean;result:unknown;diagnostics:Diagnostic[]}
interface Definition {def:{hash:string};build:{ms:number;size:number}}
let passed=0;
const total=2;
function assert(condition:unknown,message:string):asserts condition {if(!condition)throw new Error(message);}
async function operation<T>(name:string,body:unknown):Promise<T>{
  const response=await fetch(`${endpoint}/v1/${name}`,{method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`},body:JSON.stringify(body)});
  if(!response.ok)throw new Error(`${name}: HTTP ${response.status}: ${await response.text()}`);
  const reply=await response.json() as Reply;
  assert(reply.ok,JSON.stringify(reply));return reply.result as T;
}
async function define(name:string,lang:'rust',source:string,deps:Record<string,string>={}):Promise<Definition>{return operation('define',{name,lang,source,deps});}
async function call<T>(hash:string,args:unknown):Promise<T>{return operation('command',{command:'call',args:{hash,args}});}
async function check(name:string,test:()=>Promise<void>){await test();passed++;console.log(`PASS ${name}`);}
try {
  await check('Rust recurses through synchronous definition calls',async()=>{
    const definition=await define('m6-recursive','rust',await readFile(new URL('../examples/rust-recursive/src/lib.rs',import.meta.url),'utf8'));
    assert(await call<number>(definition.def.hash,[4])===4,'recursive definition call failed');
  });
  await check('new Rust source builds in under five seconds with warm dependencies',async()=>{
    const nonce=Date.now();
    await define('m6-warm-baseline','rust',`#[loom::def(effects=[])]pub fn warm()->i64{${nonce}}`);
    const measured=await define('m6-warm-measured','rust',`#[loom::def(effects=[])]pub fn warm()->i64{${nonce+1}}`);
    assert(measured.build.size>0,'no wasm module artifact');
    assert(measured.build.ms<5000,`warm build took ${measured.build.ms} ms`);
    assert(await call<number>(measured.def.hash,[])===nonce+1,'measured wasm module did not consume new source');
    console.log(`warm Rust build ${measured.build.ms} ms`);
  });
} finally {console.log(`${passed}/${total} M6 native language checks pass`);}

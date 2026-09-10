/** Supplementary native shared-core limits. Run after shared-execution.ts on its isolated daemon. */
import {readFile} from 'node:fs/promises';
import {LoomMcpClient, object} from '../mcp-client';
interface Gate {name:string;pass:boolean;detail:string}
const gates:Gate[]=[];
const names=['nested jobs exceed active worker count','lifetime job admission bound','child linear stack bounds','memory allocation bound','panic stops a spinning borrowed sibling','self-addressed effect fork'];
let client:LoomMcpClient|undefined;
function assert(value:unknown,message:string):asserts value {if(!value)throw new Error(message);}
async function define(name:string,source:string) {
  const reply=await client!.callTool('loom_define',{lang:'rust',name:`shared-limits-${name}`,source});
  assert(reply.ok,JSON.stringify(reply));
  const hash=object(object(reply.result).def).hash;
  assert(typeof hash==='string','missing definition hash');
  return hash;
}
async function call(hash:string,args:unknown[]=[]) {
  return client!.callTool('loom_command',{command:'call',args:{hash,args}});
}
async function gate(name:string,body:()=>Promise<void>) {
  try {await body();gates.push({name,pass:true,detail:'native control passed'});}
  catch(error) {gates.push({name,pass:false,detail:String(error)});}
}
let positive='';
try {
  const endpoint=process.env.LOOM_URL,tokenFile=process.env.LOOM_TOKEN_FILE;
  assert(endpoint&&tokenFile,'Set isolated LOOM_URL and LOOM_TOKEN_FILE');
  client=new LoomMcpClient({endpoint,token:(await readFile(tokenFile,'utf8')).trim()});
  await client.connect();
  await gate(names[0]!,async()=>{
    positive=await define('nested',`fn nested(depth:u32)->u32 {if depth==0 {1} else {loom::scope(|s|s.fork(||nested(depth-1)).expect("fork").join().expect("join")+1).expect("scope")}} #[loom::def(effects=[])] pub fn main()->u32 {nested(16)}`);
    const reply=await call(positive);
    assert(reply.ok&&reply.result===17,JSON.stringify(reply));
  });
  assert(gates[0]?.pass,'positive native nesting control failed');
  await gate(names[1]!,async()=>{
    const hash=await define('jobs',`#[loom::def(effects=[])] pub fn main()->u32 {loom::scope(|s| {let mut count=0; for _ in 0..513 {match s.fork(||1_u32) {Ok(job)=>{count+=job.join().expect("join");},Err(_)=>return count}} count}).expect("scope")}`);
    const reply=await call(hash);
    assert(reply.ok&&reply.result===512,'expected exactly512 admitted lifetime jobs: '+JSON.stringify(reply));
  });
  await gate(names[2]!,async()=>{
    const hash=await define('stack',`#[inline(never)] fn small()->u8 {let bytes=std::hint::black_box([7_u8;1024]);std::hint::black_box(&bytes)[0]} #[inline(never)] fn large()->u8 {let bytes=std::hint::black_box([7_u8;524288]);std::hint::black_box(&bytes)[0]} #[loom::def(effects=[])] pub fn main(big:bool)->u8 {loom::scope(|s|s.fork(||if big {large()} else {small()}).expect("fork").join().expect("join")).expect("scope")}`);
    const small=await call(hash,[false]);
    assert(small.ok&&small.result===7,'small stack positive failed: '+JSON.stringify(small));
    const large=await call(hash,[true]);
    assert(!large.ok&&/trap|unreachable|stack|cancelled/i.test(JSON.stringify(large)),'oversized child stack did not trap: '+JSON.stringify(large));
    const repeated=await call(hash,[false]);
    assert(repeated.ok&&repeated.result===7,'post-trap fresh execution failed');
  });
  await gate(names[3]!,async()=>{
    const hash=await define('memory',`#[loom::def(effects=[])] pub fn main()->bool {let mut bytes=Vec::<u8>::new();bytes.try_reserve_exact(300*1024*1024).is_err()}`);
    const reply=await call(hash);
    assert(reply.ok&&reply.result===true,'oversized allocation not refused: '+JSON.stringify(reply));
  });
  await gate(names[4]!,async()=>{
    const hash=await define('panic',`use std::sync::atomic::{AtomicU32,Ordering}; #[loom::def(effects=[])] pub fn main()->u32 {let borrowed=AtomicU32::new(0);loom::scope(|s| {let spin=s.fork(||loop {borrowed.fetch_add(1,Ordering::Relaxed);std::hint::spin_loop();}).expect("fork");let fail=s.fork(||panic!("child trap control")).expect("fork");let _:()=fail.join().expect("join");let _:()=spin.join().expect("join");0}).expect("scope")}`);
    const start=performance.now();
    const reply=await call(hash);
    assert(!reply.ok,'panicking scoped child unexpectedly returned');
    assert(performance.now()-start<5000,'child trap waited for30-second execution deadline');
    const recovery=await call(positive);
    assert(recovery.ok&&recovery.result===17,'daemon/fresh execution did not recover after child trap');
  });
  await gate(names[5]!,async()=>{
    const hash=await define('self-fork',`#[loom::def(effects=["fork","join"])] pub fn main(depth:u32)->u32 {if depth==0 {1} else {let job=loom::fork(MAIN_DEF,depth-1).expect("fork");loom::join(vec![job]).expect("join")[0]+1}}`);
    const reply=await call(hash,[2]);
    assert(reply.ok&&reply.result===3,'self-addressed fork failed: '+JSON.stringify(reply));
  });
} catch(error) {
  for(const name of names)if(!gates.some(g=>g.name===name))gates.push({name,pass:false,detail:String(error)});
} finally {
  await client?.close();
  const passed=gates.filter(g=>g.pass).length;
  console.log(JSON.stringify({stage:'shared-limits',passed,total:names.length,first_failure:gates.find(g=>!g.pass)?.name??null,gates}));
  console.log(`${passed}/${names.length} shared limit controls pass`);
  if(passed!==names.length)process.exitCode=1;
}

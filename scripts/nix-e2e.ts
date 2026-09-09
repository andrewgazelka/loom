import {mkdtemp,readFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
const directory=await mkdtemp(join(tmpdir(),'loom-nix-'));
const port=18787+Math.floor(Math.random()*1000);
const endpoint=`http://127.0.0.1:${port}`;
const daemon=Bun.spawn([process.argv[2]!,'--db',join(directory,'explicit.sqlite'),'--bind',`127.0.0.1:${port}`],{env:{...process.env,LOOM_DATA_DIR:directory,LOOM_BIND:'127.0.0.1:1',LOOM_TOKEN:undefined,CARGO_HOME:join(directory,'cargo')},stdout:'inherit',stderr:'inherit'});
let passed=0;
try {
  const deadline=Date.now()+30000;
  for (;;) {
    try {if((await fetch(endpoint)).ok) break;} catch {}
    if(daemon.exitCode!==null) throw new Error(`daemon exited ${daemon.exitCode}`);
    if(Date.now()>deadline) throw new Error('packaged daemon readiness timeout');
    await Bun.sleep(100);
  }
  const token=(await readFile(join(directory,'token'),'utf8')).trim();
  if(token.length!==64) throw new Error('persisted token missing');
  const html=await (await fetch(endpoint)).text();
  if(!html.includes('<html')) throw new Error('static UI missing');
  passed++;
  async function request(operation:string,body:unknown) {
    const response=await fetch(`${endpoint}/v1/${operation}`,{method:'POST',headers:{Authorization:`Bearer ${token}`,'Content-Type':'application/json'},body:JSON.stringify(body)});
    const reply=await response.json() as {ok:boolean;result:any};
    if(!response.ok||!reply.ok) throw new Error(JSON.stringify(reply));
    return reply.result;
  }
  for(const definition of [
    {name:'nix-ts',lang:'ts',source:'export function main(a: number, b: number): number { return a + b; }'},
    {name:'nix-rust',lang:'rust',source:'#[loom::def] pub fn main(a: i64,b: i64)->i64 { a+b }'},
  ]) {
    const result=await request('define',definition);
    const answer=await request('command',{command:'call',args:{hash:result.def.hash,args:[20,22]}});
    if(answer!==42) throw new Error(`${definition.lang} returned ${JSON.stringify(answer)}`);
    passed++;
  }
} finally {
  daemon.kill('SIGTERM');
  await daemon.exited;
  await rm(directory,{recursive:true,force:true});
  console.log(`${passed}/3 packaged execution checks pass`);
}

/** Five real registry crates, then a new body with the same dependency graph. */
import {readFile} from 'node:fs/promises';
import {LoomMcpClient,object} from '../mcp-client';
const endpoint=process.env.LOOM_URL;
const tokenFile=process.env.LOOM_TOKEN_FILE;
const names=['warm five-crate delta <=600ms','one rustc invocation','missing dependency rebuild control'];
interface Gate {gate:string;pass:boolean;detail:string}
const gates:Gate[]=[];
function record(gate:string,pass:boolean,detail:string){gates.push({gate,pass,detail});console.log(JSON.stringify({gate,pass,detail}));}
function assert(value:unknown,message:string):asserts value{if(!value)throw new Error(message);}
let client:LoomMcpClient|undefined;
try {
  assert(endpoint&&tokenFile,'LOOM_URL and LOOM_TOKEN_FILE required');
  client=new LoomMcpClient({endpoint,token:(await readFile(tokenFile,'utf8')).trim()});
  await client.connect();
  const crates=[{name:'heck',version:'0.5.0'},{name:'strsim',version:'0.11.1'},{name:'adler2',version:'2.0.1'},{name:'version_check',version:'0.9.5'},{name:'cfg-if',version:'1.0.4'}];
  const dependencies:string[]=[];
  for(const crate of crates) {
    const reply=await client.callTool('crate_add',crate);
    assert(reply.ok,JSON.stringify(reply));
    const result=object(reply.result);
    assert(typeof result.hash==='string'&&/^[0-9a-f]{64}$/.test(result.hash),'Missing crate tree hash');
    dependencies.push(`${crate.name} = { hash = "${result.hash}", features = [] }`);
  }
  const manifest=`[package]\nname="warm-five-crates"\nversion="0.1.0"\nedition="2024"\n[loom.crates]\n${dependencies.join('\n')}\n`;
  const seed=Date.now();
  function source(marker:number) {
    return `#[loom::def(effects=[])] pub fn main() -> u64 {
      assert_eq!(heck::AsSnakeCase("SomeValue").to_string(), "some_value");
      assert_eq!(strsim::levenshtein("abc", "adc"), 1);
      assert_eq!(adler2::adler32_slice(b""), 1);
      assert!(version_check::Version::parse("1.80.0").is_some());
      cfg_if::cfg_if! { if #[cfg(target_arch="wasm32")] { const TARGET: u64 = 0; } else { const TARGET: u64 = 1; } }
      ${marker} + TARGET
    }`;
  }
  async function define(marker:number) {
    const start=performance.now();
    const reply=await client!.callTool('loom_define',{lang:'rust',name:'benchmark-warm-five-crates',source:JSON.stringify({files:{'Cargo.toml':manifest,'src/lib.rs':source(marker)}})});
    const wallMs=performance.now()-start;
    assert(reply.ok,JSON.stringify(reply));
    const result=object(reply.result),definition=object(result.def);
    assert(typeof definition.hash==='string','Missing definition hash');
    const called=await client!.callTool('loom_command',{command:'call',args:{hash:definition.hash,args:[]}});
    assert(called.ok&&called.result===marker,`Fresh compiled result mismatch: ${JSON.stringify(called)}`);
    return {wallMs,hash:definition.hash,build:object(result.build)};
  }
  const first=await define(seed);
  const changed=await define(seed+1);
  assert(first.hash!==changed.hash,'Body edit did not produce a fresh definition');
  console.log(JSON.stringify({stage:'warm-crates',first,changed}));
  record(names[0]!,changed.wallMs<=600,`${changed.wallMs.toFixed(3)}ms for five-crate body edit, fresh result verified`);
  const count=changed.build.rustc_invocations;
  record(names[1]!,Number.isSafeInteger(count)&&count===1,typeof count==='number'?`${count} measured rustc invocations`:'build response has no rustc_invocations measurement');
  if(process.env.LOOM_BENCH_DB&&process.env.LOOM_ARTIFACT_EVICTOR) {
    const logsRef=changed.build.logs_ref;
    assert(typeof logsRef==='string'&&/^[0-9a-f]{64}$/.test(logsRef),'Missing immutable build log reference');
    const child=Bun.spawn([process.env.LOOM_ARTIFACT_EVICTOR,process.env.LOOM_BENCH_DB,'heck','wasm32-unknown-unknown',logsRef],{stdout:'pipe',stderr:'pipe'});
    const eviction={stdout:'',stderr:'',exit:-1};
    await Promise.all([
      new Response(child.stdout).text().then(value=>{eviction.stdout=value;}),
      new Response(child.stderr).text().then(value=>{eviction.stderr=value;}),
      child.exited.then(value=>{eviction.exit=value;}),
    ]);
    assert(eviction.exit===0,`Artifact eviction failed: ${eviction.stderr}`);
    const removed=object(JSON.parse(eviction.stdout));
    assert(removed.name==='heck'&&removed.target==='wasm32-unknown-unknown'&&removed.build_log_hash===logsRef&&typeof removed.dependency_graph==='string'&&/^[0-9a-f]{64}$/.test(removed.dependency_graph)&&typeof removed.key==='string'&&Number.isSafeInteger(removed.removed_outputs)&&Number(removed.removed_outputs)>0,'Evictor did not verify removal from this build graph and compiler target');
    const rebuilt=await define(seed+2);
    const count=rebuilt.build.rustc_invocations;
    record(names[2]!,count===2,`${String(count)} compilations after removing heck CAS outputs and materializations; expected dependency + root; fresh result verified`);
  } else {
    record(names[2]!,false,'LOOM_BENCH_DB and LOOM_ARTIFACT_EVICTOR are required for the isolated missing-dependency experiment');
  }
} catch(error) {
  console.error(String(error));
} finally {
  if(client)try{await client.close();}catch(error){console.error(String(error));}
  for(const gate of names)if(!gates.some(value=>value.gate===gate))record(gate,false,'no execution witness');
  // This process reports each verdict independently; the outer goal owns exit status.
}

/** Integration goal command. Every pass comes from a fresh production execution. */
import {resolve} from 'node:path';

interface Gate {name:string;pass:boolean;detail:string}
interface Invocation {exit:number;stdout:string;stderr:string}
const gates:Gate[]=[];
const fixture=process.argv[2];
const native=process.argv[3];
const scanNames=[
  'native correctness','scoped correctness','changing winners',
  ...['scoped'].flatMap(name=>[
    `${name}: median <15ms`,
    `${name}: identical-scan database growth <32768 bytes`,
    `${name}: wire bytes <200000`,
    `${name}: median reply storage wait <1ms`,
  ]),
];
const compilerNames=['warm five-crate delta <=600ms','one rustc invocation','missing dependency rebuild control'];
const names=[...scanNames,...compilerNames];
function record(name:string,pass:boolean,detail:string) {
  gates.push({name,pass,detail});
  console.log(JSON.stringify({gate:name,pass,detail}));
}
async function invoke(script:string,args:string[],extra:Record<string,string>={}):Promise<Invocation> {
  return invokeCommand(['bun',script,...args],extra);
}
async function invokeCommand(command:string[],extra:Record<string,string>={}):Promise<Invocation> {
  const child=Bun.spawn(command,{env:{...process.env,...extra},stdout:'pipe',stderr:'pipe'});
  const result:Invocation={exit:-1,stdout:'',stderr:''};
  await Promise.all([
    new Response(child.stdout).text().then(value=>{result.stdout=value;}),
    new Response(child.stderr).text().then(value=>{result.stderr=value;}),
    child.exited.then(value=>{result.exit=value;}),
  ]);
  return result;
}
function scanWitness(stdout:string):Gate[]|undefined {
  const summaries:unknown[]=[];
  for(const line of stdout.split('\n')) {
    try {const value=JSON.parse(line);if(value?.stage==='goal-summary')summaries.push(value);} catch {}
  }
  if(summaries.length!==1)return;
  const value=summaries[0] as Record<string,unknown>;
  if(!Array.isArray(value.gates)||value.gates.length!==scanNames.length||value.total!==scanNames.length)return;
  const seen=new Set<string>();
  const validated:Gate[]=[];
  for(const candidate of value.gates) {
    if(!candidate||typeof candidate!=='object')return;
    const item=candidate as Record<string,unknown>;
    if(typeof item.name!=='string'||!scanNames.includes(item.name)||seen.has(item.name)||typeof item.pass!=='boolean'||typeof item.detail!=='string')return;
    seen.add(item.name);validated.push({name:item.name,pass:item.pass,detail:item.detail});
  }
  if(value.passed!==validated.filter(gate=>gate.pass).length)return;
  if(value.first_failure!==(validated.find(gate=>!gate.pass)?.name??null))return;
  return validated;
}
async function scan() {
  const result=await invoke('scripts/bench/largest.ts',[resolve(fixture!),resolve(native!)]);
  process.stdout.write(result.stdout);
  process.stderr.write(result.stderr);
  const witness=scanWitness(result.stdout);
  const consistent=witness!==undefined&&result.exit===(witness.every(gate=>gate.pass)?0:1);
  for(const name of scanNames) {
    const gate=witness?.find(gate=>gate.name===name);
    record(name,consistent&&gate?.pass===true,consistent?gate!.detail:`missing, invalid, or inconsistent scan goal summary; exit ${result.exit}`);
  }
}

try {
  if(!fixture||!native||!process.env.LOOM_URL||!process.env.LOOM_TOKEN_FILE) {
    throw new Error('Usage: LOOM_URL=isolated-url LOOM_TOKEN_FILE=file bun scripts/bench/unified-memory.ts FIXTURE NATIVE_BINARY');
  }
  await scan();
  // The build lane supplies an executable witness, not a receipt from an older run.
  const buildScript='scripts/bench/warm-crates.ts';
  if(await Bun.file(buildScript).exists()) {
    let result:Invocation;
    if(process.env.LOOM_BUILD_BENCH_COMMAND) {
      const command:unknown=JSON.parse(process.env.LOOM_BUILD_BENCH_COMMAND);
      if(!Array.isArray(command)||command.length===0||!command.every(value=>typeof value==='string'&&value.length>0))throw new Error('LOOM_BUILD_BENCH_COMMAND must be a nonempty JSON array of command arguments');
      result=await invokeCommand(command);
    } else {
      result=await invoke(buildScript,[]);
    }
    process.stdout.write(result.stdout);process.stderr.write(result.stderr);
    for(const name of compilerNames) {
      let witness:Gate|undefined;
      for(const line of result.stdout.split('\n')) {
        try {const item=JSON.parse(line);if(item.gate===name&&typeof item.pass==='boolean'&&typeof item.detail==='string')witness={name,pass:item.pass,detail:item.detail};}catch{}
      }
      record(name,result.exit===0&&witness?.pass===true,witness?.detail??`missing build witness; exit ${result.exit}`);
    }
  }
} catch(error) {
  console.error(String(error));
} finally {
  for(const name of names) {
    if(!gates.some(gate=>gate.name===name))record(name,false,'no execution witness');
  }
  const passed=gates.filter(gate=>gate.pass).length;
  const first=gates.find(gate=>!gate.pass);
  console.log(`${passed}/${names.length} unified-memory gates pass; first failing step: ${first?.name??'none'}`);
  process.exitCode=passed===names.length?0:1;
}

/** Integration goal command. Every pass comes from a fresh production execution. */
import {resolve} from 'node:path';

interface Gate {name:string;pass:boolean;detail:string}
interface VariantMeasurement {name:string;median_ms:number;samples_ms:number[];queued_recording_commits?:(number|null)[]}
interface ScanSummary {stage:string;variants:VariantMeasurement[]}
interface Invocation {exit:number;stdout:string;stderr:string}
const gates:Gate[]=[];
const fixture=process.argv[2];
const native=process.argv[3];
const names=['legacy correctness','all <=55ms','fork <=70ms','queued recording commits <=3','warm five-crate delta <=600ms','one rustc invocation','missing dependency rebuild control'];
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
function summary(stdout:string):ScanSummary|undefined {
  for(const line of stdout.split('\n')) {
    try {const value=JSON.parse(line);if(value.stage==='warm-summary')return value;} catch {}
  }
}
async function scan(variants:string[],correctness:string,targets:Record<string,number>) {
  const result=await invoke('scripts/bench/largest.ts',[resolve(fixture!),resolve(native!)],{LOOM_BENCH_VARIANTS:variants.join(',')});
  process.stdout.write(result.stdout);
  process.stderr.write(result.stderr);
  const measurement=summary(result.stdout);
  const total=variants.length+2;
  const correct=result.exit===0&&result.stdout.includes(`${total}/${total} scan benchmark checks pass`)&&!!measurement;
  record(correctness,correct,correct?'native tie/symlink control and seven changing winners match':`scan exit ${result.exit}; ${result.stderr.slice(-1200)}`);
  if(variants.includes('all')) {
    const samples=measurement?.variants.flatMap(value=>value.queued_recording_commits??[]);
    const valid=!!samples&&samples.length===variants.length*7&&samples.every(value=>typeof value==='number'&&Number.isSafeInteger(value)&&value>=0);
    record('queued recording commits <=3',correct&&valid&&samples!.every(value=>value!<=3),valid?`${JSON.stringify(samples)} queued recording commits per warm scan`:'missing or invalid recording_commits stats counter');
  }
  for(const variant of variants) {
    const measured=measurement?.variants.find(value=>value.name===variant);
    const limit=targets[variant]!;
    const gate=variant==='all'?'all <=55ms':'fork <=70ms';
    const valid=!!measured&&Number.isFinite(measured.median_ms)&&measured.samples_ms.length===7&&measured.samples_ms.every(value=>Number.isFinite(value)&&value>=0);
    record(gate,correct&&valid&&measured!.median_ms<=limit,valid?`${measured!.median_ms.toFixed(3)}ms median / ${limit}ms target`:'no validated warm samples');
  }
}
try {
  if(!fixture||!native||!process.env.LOOM_URL||!process.env.LOOM_TOKEN_FILE) {
    throw new Error('Usage: LOOM_URL=isolated-url LOOM_TOKEN_FILE=file bun scripts/bench/unified-memory.ts FIXTURE NATIVE_BINARY');
  }
  await scan(['all','fork'],'legacy correctness',{all:55,fork:70});
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
    for(const name of names.slice(4)) {
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

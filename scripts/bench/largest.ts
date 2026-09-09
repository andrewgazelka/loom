/** Run only against an isolated loomd. Uses the production MCP transport and guest SDK. */
import {readFile, writeFile, mkdir, rm} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {loadavg} from 'node:os';
import {LoomMcpClient, object} from '../mcp-client';

const fixture = resolve(process.argv[2] ?? '');
const native = process.argv[3];
if (!process.argv[2] || !native || !process.env.LOOM_URL || !process.env.LOOM_TOKEN_FILE) {
  throw new Error('Usage: LOOM_URL=isolated-url LOOM_TOKEN_FILE=file bun scripts/bench/largest.ts FIXTURE NATIVE_BINARY');
}
const client = new LoomMcpClient({endpoint:process.env.LOOM_URL, token:(await readFile(process.env.LOOM_TOKEN_FILE,'utf8')).trim()});
interface Winner {path:string;size:number}
interface Measurement extends Winner {ms:number;wall_ms:number;files:number;directories:number}
interface Variant {name:string;hash:string;samples:number[];recordingCommits:(number|null)[];growthBytes:number[];usedGrowthBytes:number[];wireBytes:number[];storageMs:number[]}
function assert(value:unknown, message:string):asserts value {if(!value)throw new Error(message);}
async function nativeScan():Promise<Measurement> {
  const start=performance.now();
  const process=Bun.spawn([native!,fixture],{stdout:'pipe',stderr:'pipe'});
  const [stdout,stderr,exit]=await Promise.all([new Response(process.stdout).text(),new Response(process.stderr).text(),process.exited]);
  assert(exit===0,`Native scan failed: ${stderr}`);
  const fields=stdout.trimEnd().split('\t');
  assert(fields.length===5,'Invalid native result');
  const result={ms:Number(fields[0]),size:Number(fields[1]),files:Number(fields[2]),directories:Number(fields[3]),path:fields[4]!,wall_ms:performance.now()-start};
  assert([result.ms,result.size,result.files,result.directories].every(Number.isFinite),'Invalid native numbers');
  return result;
}
function same(actual:unknown, expected:Winner) {
  const result=object(actual);
  assert(result.path===expected.path&&result.size===expected.size,`Winner mismatch: ${JSON.stringify({actual,expected})}`);
}
async function command(command:string,args:unknown) {
  const reply=await client.callTool('loom_command',{command,args});
  assert(reply.ok,JSON.stringify(reply));return reply.result;
}
const variants:Variant[]=[];
const mutation=join(fixture,'nested/middle/deep/benchmark-child');
let created=false;
let passed=0;
const failures:string[]=[];
interface Gate {name:string;pass:boolean;detail:string}
const gates:Gate[]=[];
const totalChecks=12;
function gate(ok:boolean, name:string,detail='') {gates.push({name,pass:ok,detail});if(ok)passed++;else failures.push(name);}
function counter(stats:Record<string,unknown>, key:string):number {const value=stats[key];return typeof value==='number' && Number.isFinite(value) && value>=0 ? value : Number.NaN;}
try {
  await client.connect();
  const baseline=await nativeScan();
  assert(baseline.files===10000&&baseline.directories===256,'Expected the standard isolated 10,000-file fixture');
  assert(baseline.path==='nested/middle/deep/alpha.dat'&&baseline.size===8193,'Tie/symlink fixture control failed');
  gate(true,'native correctness','10000 files, tie and symlink control');
  const machine=object(await command('machine.create',{root:fixture}));
  const existing=process.env.LOOM_BENCH_HASHES?object(JSON.parse(process.env.LOOM_BENCH_HASHES)):{};
  for(const name of ['all','fork']) {
    let hash=existing[name];
    if(hash===undefined) {
    const source=await readFile(new URL(`largest-${name}.rs`,import.meta.url),'utf8');
    const start=performance.now();
    const reply=await client.callTool('loom_define',{lang:'rust',name:`benchmark-largest-${name}`,source});
    assert(reply.ok,JSON.stringify(reply));
    const result=object(reply.result),def=object(result.def);
    assert(typeof def.hash==='string','Missing definition hash');
    console.log(JSON.stringify({stage:'define',name,wall_ms:performance.now()-start,build:result.build}));
    hash=def.hash;
    } else {
      console.log(JSON.stringify({stage:'reuse-compiled-definition',name,hash}));
    }
    assert(typeof hash==='string'&&/^[0-9a-f]{64}$/.test(hash),'Invalid definition hash');
    variants.push({name,hash,samples:[],recordingCommits:[],growthBytes:[],usedGrowthBytes:[],wireBytes:[],storageMs:[]});
    const callStart=performance.now();
    same(await command('call',{hash,args:[machine.id,'.']}),baseline);
    console.log(JSON.stringify({stage:'first-call',name,ms:performance.now()-callStart}));
    gate(true,`${name} correctness`,'first result matches native');
  }
  await mkdir(mutation);created=true;
  const nativeSamples:number[]=[],nativeWall:number[]=[];
  for(let round=0;round<7;round++) {
    await writeFile(join(mutation,'winner.dat'),new Uint8Array(16387+round));
    const expected={path:'nested/middle/deep/benchmark-child/winner.dat',size:16387+round};
    // Rotate order; every variant checks the changed winner, never a cached old answer.
    const names=round%2===0?['native','all','fork']:['fork','all','native'];
    for(const name of names) {
      if(name==='native') {
        const result=await nativeScan();same(result,expected);
        assert(result.files===10001&&result.directories===257,'Native scan counts changed');
        nativeSamples.push(result.ms);nativeWall.push(result.wall_ms);
      } else {
        const variant=variants.find(value=>value.name===name)!;
        const beforeStats=object(await command('stats',{}));
        const before=beforeStats.recording_commits;
        const start=performance.now();
        same(await command('call',{hash:variant.hash,args:[machine.id,'.']}),expected);
        variant.samples.push(performance.now()-start);
        const afterStats=object(await command('stats',{}));
        const after=afterStats.recording_commits;
        variant.wireBytes.push(counter(afterStats,'effect_wire_bytes')-counter(beforeStats,'effect_wire_bytes'));
        variant.storageMs.push(counter(afterStats,'last_reply_storage_nanos')/1e6);
        variant.recordingCommits.push(typeof before==='number'&&typeof after==='number'&&Number.isSafeInteger(before)&&Number.isSafeInteger(after)&&after>=before ? after-before : null);
      }
    }
  }
  gate(true,'changing winners','all seven changes match native for both variants');
  function median(samples:number[]) {return [...samples].sort((a,b)=>a-b)[Math.floor(samples.length/2)]!;}
  // Growth is measured on identical scans after the changing-winner controls.
  // SQLite allocated pages include indexes; CAS payload bytes alone are insufficient.
  for(const variant of variants) {
    const before=object(await command('stats',{}));
    for(let round=0;round<7;round++) {
      same(await command('call',{hash:variant.hash,args:[machine.id,'.']}),{path:'nested/middle/deep/benchmark-child/winner.dat',size:16393});
    }
    const after=object(await command('stats',{}));
    variant.growthBytes.push((counter(after,'database_bytes')-counter(before,'database_bytes'))/7);
    variant.usedGrowthBytes.push(((counter(after,'database_bytes')-counter(after,'reusable_database_bytes'))-(counter(before,'database_bytes')-counter(before,'reusable_database_bytes')))/7);
    gate(median(variant.samples)<15,`${variant.name}: median <15ms`,`${median(variant.samples)}ms`);
    gate([...variant.growthBytes,...variant.usedGrowthBytes].every(value=>Number.isFinite(value)&&value>=0&&value<32768),`${variant.name}: identical-scan database growth <32768 bytes`,JSON.stringify({allocated_average:variant.growthBytes,used_average:variant.usedGrowthBytes}));
    gate(variant.wireBytes.every(value=>Number.isFinite(value)&&value>=0&&value<200000),`${variant.name}: wire bytes <200000`,JSON.stringify(variant.wireBytes));
    gate(variant.storageMs.every(value=>Number.isFinite(value)&&value>=0)&&median(variant.storageMs)<1,`${variant.name}: median reply storage wait <1ms`,JSON.stringify(variant.storageMs));
  }
  const nativeMedian=median(nativeSamples);
  console.log(JSON.stringify({stage:'warm-summary',load_average:loadavg(),native:{samples_ms:nativeSamples,median_ms:nativeMedian,wall_samples_ms:nativeWall,wall_median_ms:median(nativeWall)},variants:variants.map(variant=>({name:variant.name,samples_ms:variant.samples,queued_recording_commits:variant.recordingCommits,database_bytes_per_identical_scan:variant.growthBytes,used_database_bytes_per_identical_scan:variant.usedGrowthBytes,wire_bytes:variant.wireBytes,reply_storage_ms:variant.storageMs,median_ms:median(variant.samples),ratio_to_native:median(variant.samples)/nativeMedian}))}));
} finally {
  if(created)await rm(mutation,{recursive:true});
  await client.close();
  console.log(JSON.stringify({stage:'goal-summary',gates,passed,total:totalChecks,first_failure:failures[0]??(passed===totalChecks?null:'incomplete execution')}));
  console.log(`${passed}/${totalChecks} scan benchmark checks pass`);
  for(const failure of failures)console.error(`FAIL: ${failure}`);
  if(passed!==totalChecks)process.exitCode=1;
}

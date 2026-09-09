/** Run only against an isolated loomd. Uses the production MCP transport and guest SDK. */
import {readFile, writeFile, mkdir, rm} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {LoomMcpClient, object} from '../mcp-client';

const fixture = resolve(process.argv[2] ?? '');
const native = process.argv[3];
if (!process.argv[2] || !native || !process.env.LOOM_URL || !process.env.LOOM_TOKEN_FILE) {
  throw new Error('Usage: LOOM_URL=isolated-url LOOM_TOKEN_FILE=file bun scripts/bench/largest.ts FIXTURE NATIVE_BINARY');
}
const client = new LoomMcpClient({endpoint:process.env.LOOM_URL, token:(await readFile(process.env.LOOM_TOKEN_FILE,'utf8')).trim()});
interface Winner {path:string;size:number}
interface Measurement extends Winner {ms:number;wall_ms:number;files:number;directories:number}
interface Variant {name:string;hash:string;samples:number[];recordingCommits:(number|null)[]}
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
try {
  await client.connect();
  const baseline=await nativeScan();
  assert(baseline.files===10000&&baseline.directories===256,'Expected the standard isolated 10,000-file fixture');
  assert(baseline.path==='nested/middle/deep/alpha.dat'&&baseline.size===8193,'Tie/symlink fixture control failed');
  passed++;
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
    variants.push({name,hash,samples:[],recordingCommits:[]});
    const callStart=performance.now();
    same(await command('call',{hash,args:[machine.id,'.']}),baseline);
    console.log(JSON.stringify({stage:'first-call',name,ms:performance.now()-callStart}));
    passed++;
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
        const before=object(await command('stats',{})).recording_commits;
        const start=performance.now();
        same(await command('call',{hash:variant.hash,args:[machine.id,'.']}),expected);
        variant.samples.push(performance.now()-start);
        const after=object(await command('stats',{})).recording_commits;
        variant.recordingCommits.push(typeof before==='number'&&typeof after==='number'&&Number.isSafeInteger(before)&&Number.isSafeInteger(after)&&after>=before ? after-before : null);
      }
    }
  }
  passed++;
  function median(samples:number[]) {return [...samples].sort((a,b)=>a-b)[Math.floor(samples.length/2)]!;}
  const nativeMedian=median(nativeSamples);
  console.log(JSON.stringify({stage:'warm-summary',native:{samples_ms:nativeSamples,median_ms:nativeMedian,wall_samples_ms:nativeWall,wall_median_ms:median(nativeWall)},variants:variants.map(variant=>({name:variant.name,samples_ms:variant.samples,queued_recording_commits:variant.recordingCommits,median_ms:median(variant.samples),ratio_to_native:median(variant.samples)/nativeMedian}))}));
} finally {
  if(created)await rm(mutation,{recursive:true});
  await client.close();
  console.log(`${passed}/4 scan benchmark checks pass`);
}

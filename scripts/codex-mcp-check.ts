/** Independent verifier: the caller owns isolated loomd and the actual fresh Codex invocation. */
import {mkdir, readdir, readFile, stat, writeFile, symlink, rename, readlink, lstat, mkdtemp, rm} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {LoomMcpClient, object} from './mcp-client';
const args=process.argv.slice(2);
function option(name:string):string|undefined { const index=args.indexOf(name); return index<0?undefined:args[index+1]; }
function requireValue(value:string|undefined,label:string):string { if(!value)throw new Error(`Missing ${label}`);return value; }
interface Largest {path:string;size:number}
async function scan(root:string):Promise<{largest:Largest;files:number;directories:number}> {
  let largest:Largest={path:'',size:-1},files=0,directories=0;
  async function walk(relative:string):Promise<void> {
    for(const entry of await readdir(join(root,relative),{withFileTypes:true})) {
      const path=relative?`${relative}/${entry.name}`:entry.name;
      if(entry.isDirectory()){directories++;await walk(path);}
      else if(entry.isFile()){files++;const size=(await stat(join(root,path))).size;if(size>largest.size||(size===largest.size&&path<largest.path))largest={path,size};}
    }
  }
  await walk('');return {largest,files,directories};
}
async function createFixture(root:string):Promise<void> {
  await mkdir(root,{recursive:true});
  if((await readdir(root)).length)throw new Error('Fixture directory must be empty');
  for(let directory=0;directory<253;directory++) {
    const path=join(root,`d${String(directory).padStart(3,'0')}`);await mkdir(path);
    const files=directory===252?0:39+(directory<172?1:0);
    await Promise.all(Array.from({length:files},(_,index)=>writeFile(join(path,`f${String(index).padStart(3,'0')}.dat`),new Uint8Array(index+1))));
  }
  const deep=join(root,'nested/middle/deep');await mkdir(deep,{recursive:true});
  await rename(join(root,'d000/f000.dat'),join(deep,'alpha.dat'));
  await rename(join(root,'d000/f001.dat'),join(deep,'zeta.dat'));
  await writeFile(join(deep,'alpha.dat'),new Uint8Array(8193));
  await writeFile(join(deep,'zeta.dat'),new Uint8Array(8193));
  const external=`${root}.external`;await writeFile(external,new Uint8Array(65537),{flag:'wx'});
  await symlink(external,join(root,'external-link'));
  await symlink('.',join(root,'cycle-link'));
  await symlink('missing-target',join(root,'broken-link'));
  console.log(JSON.stringify({fixture:root,...await scan(root)}));
}
if(args.includes('--baseline')) {console.log('0/4 fresh Codex recursive MCP checks pass; first failing step: real Codex trace not supplied');process.exit(1);}
if(option('--create-fixture')) {await createFixture(resolve(requireValue(option('--create-fixture'),'fixture')));process.exit(0);}
let passed=0;
let firstFailure: string | undefined;
let client:LoomMcpClient|undefined;
let mutationRoot:string|undefined;
async function gate(name:string,run:()=>Promise<void>){try {await run();passed++;console.log(`PASS ${name}`);} catch(error) { firstFailure ??= `${name}: ${String(error)}`; throw error; }}
function assert(condition:unknown,message:string):asserts condition {if(!condition)throw new Error(message);}
function decodedLoomResult(call:Record<string,unknown>):Record<string,unknown>|undefined {
  if(call.status!=='completed'||call.error||!call.result)return undefined;
  const result=object(call.result);
  if(result.isError===true||!Array.isArray(result.content))return undefined;
  const text=result.content.map(value=>object(value)).find(value=>value.type==='text');
  if(typeof text?.text!=='string')return undefined;
  const reply=object(JSON.parse(text.text));return reply.ok===true?reply:undefined;
}
function successfulLoomResult(call:Record<string,unknown>):boolean {return decodedLoomResult(call)!==undefined;}
try {
  const root=resolve(requireValue(option('--fixture'),'--fixture'));
  const trace=requireValue(option('--trace'),'--trace');
  const endpoint=requireValue(process.env.LOOM_URL,'LOOM_URL');
  const token=requireValue(process.env.LOOM_TOKEN,'LOOM_TOKEN');
  const records=(await readFile(trace,'utf8')).trim().split('\n').map(line=>object(JSON.parse(line)));
  const calls=records.filter(row=>row.type==='item.completed').map(row=>object(row.item)).filter(item=>item.type==='mcp_tool_call');
  let expected=await scan(root);
  await gate('fresh Codex trace and recursive fixture',async()=>{
    assert(records.filter(row=>row.type==='thread.started').length===1&&records.some(row=>row.type==='turn.completed'),'trace is not a completed fresh Codex turn');
    assert(!records.some(row=>row.type==='turn.failed'),'Codex turn failed');
    assert(expected.files===10000&&expected.directories===256,'fixture must contain10000regularfiles and256directories');
    const items=records.filter(row=>row.type==='item.started'||row.type==='item.completed').map(row=>object(row.item));
    assert(items.every(item=>['agent_message','reasoning','plan','todo_list'].includes(String(item.type))||(item.type==='mcp_tool_call'&&item.server==='loom')),'transcript used a non-Loom tool');
    assert((await readdir(join(root,'d252'))).length===0,'empty-directory control missing');
    assert((await stat(join(root,'nested/middle/deep/alpha.dat'))).size===8193&&(await stat(join(root,'nested/middle/deep/zeta.dat'))).size===8193,'deep tied winners missing');
    for(const name of ['cycle-link','external-link','broken-link'])assert((await lstat(join(root,name))).isSymbolicLink(),`${name} control missing`);
    assert(await readlink(join(root,'cycle-link'))==='.'&&(await stat(join(root,'external-link'))).size>8193,'symlink controls invalid');
    assert(calls.some(call=>call.server==='loom'&&successfulLoomResult(call)),'no completed actual Loom MCP calls');
  });
  client=new LoomMcpClient({endpoint,token});await client.connect();
  async function command(command:string,args:unknown):Promise<unknown>{const response=await client!.callTool('loom_command',{command,args});assert(response.ok,JSON.stringify(response));return response.result;}
  const machine=object(await command('machine.create',{root}));assert(typeof machine.id==='string','machine id missing');
  const hashes:Record<string,string>={};
  async function invoke(lang:string):Promise<void>{
    const result=object(await command('call',{hash:hashes[lang],args:[machine.id,'.']}));
    const path=typeof result.path==='string'?result.path.replace(/^\.\//,''):undefined;
    assert(path===expected.largest.path&&result.size===expected.largest.size,`${lang} expected${JSON.stringify(expected.largest)}, got${JSON.stringify(result)}`);
  }
  for(const lang of ['rust'])await gate(`${lang} model-written recursive definition executes correctly`,async()=>{
    const definitions=calls.filter(call=>call.server==='loom'&&call.tool==='loom_define'&&successfulLoomResult(call)&&object(call.arguments).name===`codex-recursive-${lang}`);
    assert(definitions.length>0,`trace lacks successful${lang}definitiontool`);
    const traced=object(object(decodedLoomResult(definitions.at(-1)!)!.result).def);
    assert(typeof traced.hash==='string'&&traced.lang===lang,'accepted trace definition identity missing');
    const definition=object(await command('resolve',{hash:traced.hash}));
    assert(definition.hash===traced.hash,'resolved definition differs from accepted trace hash');
    assert(definition.lang===lang&&typeof definition.hash==='string',`accepted${lang}definitionmissing`);hashes[lang]=definition.hash;
    assert(calls.some(call=>call.server==='loom'&&call.tool==='loom_command'&&successfulLoomResult(call)&&object(call.arguments).command==='call'&&object(object(call.arguments).args).hash===definition.hash),`trace lacks actual${lang}execution`);
    await invoke(lang);
  });
  let mutationFile:string;
  await gate('Rust observes a new deeper directory and winner',async()=>{
    mutationRoot=await mkdtemp(join(root,'nested/middle/deep','.loom-check-'));
    const mutationDirectory=join(mutationRoot,'deeper');
    await mkdir(mutationDirectory);
    mutationFile=join(mutationDirectory,'winner.dat');
    await writeFile(mutationFile,new Uint8Array(16387));expected=await scan(root);await invoke('rust');
  });
  const failures: string[] = [];
  for(const lang of ['rust'])try { await gate(`${lang} five fresh warm scans median below1500ms`,async()=>{
    const samples:number[]=[];
    for(let iteration=0;iteration<5;iteration++){
      await writeFile(mutationFile!,new Uint8Array(16388+iteration));expected=await scan(root);
      const start=performance.now();await invoke(lang);samples.push(performance.now()-start);
    }
    const median=[...samples].sort((a,b)=>a-b)[2]!;console.log(JSON.stringify({lang,samples_ms:samples,median_ms:median}));assert(median<1500,`${lang} median${median}ms exceeds1500ms`);
  }); } catch(error) { failures.push(String(error)); console.error(error); }
  if(failures.length)throw new Error(failures.join('; '));
}catch(error){firstFailure ??= String(error);console.error(error);process.exitCode=1;}
finally {try {if(mutationRoot)await rm(mutationRoot,{recursive:true});} catch(error) {firstFailure ??= `fixture cleanup: ${String(error)}`;console.error(error);process.exitCode=1;} try {await client?.close();} catch(error) {console.error(error);process.exitCode=1;} console.log(`${passed}/4 fresh Codex recursive MCP checks pass${firstFailure ? `; first failing step: ${firstFailure}` : ''}`);}

/** Explicit local setup; preserves all non-Loom configuration verbatim. */
import {readFile,writeFile,rename,stat,mkdir} from 'node:fs/promises';
import {dirname,join} from 'node:path';
import {homedir} from 'node:os';
const args=process.argv.slice(2);
const option=(key:string)=>{const index=args.indexOf(key);return index<0?undefined:args[index+1];};
const tokenFile=option('--token-file');
if(!tokenFile)throw new Error('Usage: bun scripts/configure-codex-mcp.ts --token-file PATH [--url http://localhost:8787/mcp] [--config PATH]');
const metadata=await stat(tokenFile);
if(!metadata.isFile()||(metadata.mode&0o077)!==0)throw new Error('Token file must be a regular file readable only by its owner (chmod600)');
const token=(await readFile(tokenFile,'utf8')).trim();
if(!token||/[\r\n]/.test(token))throw new Error('Token file contains no valid single-line token');
const path=option('--config')??join(process.env.CODEX_HOME??join(homedir(),'.codex'),'config.toml');
const url=new URL(option('--url')??'http://127.0.0.1:8787/mcp');
if(url.protocol!=='https:'&&!(url.protocol==='http:'&&['127.0.0.1','localhost','[::1]'].includes(url.hostname)))throw new Error('Remote MCP requires HTTPS');
let previous='';try{previous=await readFile(path,'utf8');}catch(error){if((error as NodeJS.ErrnoException).code!=='ENOENT')throw error;}
Bun.TOML.parse(previous);
let skipping=false;
const retained=previous.split('\n').filter(line=>{
  const section=line.match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/);
  if(section)skipping=/^mcp_servers\.(?:loom|"loom")\s*(?:\.|$)/.test(section[1]!);
  return !skipping;
}).join('\n').trimEnd();
const tools=['loom_add', 'loom_view', 'loom_update', 'loom_history', 'loom_diff', 'loom_run', 'loom_find', 'loom_dependents', 'loom_command', 'actor_list', 'actor_tree', 'actor_info', 'actor_send', 'actor_spawn', 'actor_stop', 'actor_restart', 'actor_promote', 'actor_promote_where', 'actor_lineage', 'actor_dead_letters', 'actor_fork', 'actor_validate', 'actor_sql', 'actor_whereis', 'actor_register', 'actor_members', 'actor_behaviors', 'actor_run'];
const table=['[mcp_servers.loom]','tool_timeout_sec = 180',`url = ${JSON.stringify(url.href)}`,'[mcp_servers.loom.http_headers]',`Authorization = ${JSON.stringify(`Bearer ${token}`)}`,...tools.flatMap(tool=>[`[mcp_servers.loom.tools.${tool}]`,'approval_mode = "approve"'])].join('\n');
const updated=`${retained}\n\n${table}\n`;
const parsed=Bun.TOML.parse(updated) as {mcp_servers?:{loom?:{url?:string;tools?:Record<string,{approval_mode?:string}>}}};
if(parsed.mcp_servers?.loom?.url!==url.href||Object.keys(parsed.mcp_servers.loom.tools??{}).length!==tools.length)throw new Error('Generated Loom configuration failed validation');
await mkdir(dirname(path),{recursive:true});
const temporary=`${path}.loom-${crypto.randomUUID()}`;
await writeFile(temporary,updated,{mode:0o600,flag:'wx'});await rename(temporary,path);
console.log(`Configured Loom MCP at ${url.origin}${url.pathname}; ${tools.length} authorized tools; config ${path}`);

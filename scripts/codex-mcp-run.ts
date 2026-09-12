/** Fresh Codex run against an explicitly supplied isolated loomd. Never selects the live app by default. */
import {mkdtemp,readFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
const endpoint=process.env.LOOM_URL;
const token=process.env.LOOM_TOKEN??(process.env.LOOM_TOKEN_FILE?(await readFile(process.env.LOOM_TOKEN_FILE,'utf8')).trim():undefined);
if(!endpoint||!token)throw new Error('Set LOOM_URL and LOOM_TOKEN or LOOM_TOKEN_FILE for an isolated test daemon');
const work=await mkdtemp(join(tmpdir(),'loom-codex-run-'));
const fixture=join(work,'tree');
const script=(name:string)=>new URL(name,import.meta.url).pathname;
async function execute(args:string[],options:Partial<Parameters<typeof Bun.spawn>[1]>={}){const process=Bun.spawn(args,{stdout:'inherit',stderr:'inherit',...options});const code=await process.exited;if(code!==0)throw new Error(`Child exited ${code}; logs ${work}`);}
await execute(['bun',script('codex-mcp-check.ts'),'--create-fixture',fixture]);
const args=['codex','exec','--ignore-user-config','--ephemeral','--json','--skip-git-repo-check','-C',work,'-s','read-only','-c',`mcp_servers.loom.url=${JSON.stringify(`${endpoint.replace(/\/$/,'')}/mcp`)}`,'-c','mcp_servers.loom.tool_timeout_sec=180','-c','mcp_servers.loom.bearer_token_env_var="LOOM_CODEX_TOKEN"'];
for(const tool of ['add','run','view','command'])args.push('-c',`mcp_servers.loom.tools.${tool}.approval_mode="approve"`);
args.push(`Use only Loom MCP tools, no shell/web/other tools. Create machine with command machine.create args {root:${JSON.stringify(fixture)}}. Write a recursive largest regular-file definition named codex-recursive-rust using add {source,name}. It accepts (machine:string,path:string) and returns {path:string,size:number}, path relative to supplied root without ./ prefix. Skip symlinks and special files. Traverse every nested directory; ties choose lexicographically smallest path. Call it via run {target:hash,args:[machineId,"."]} and verify result.output. Retry compiler diagnostics until it works. Final JSON {rust_hash}.
Rust single-file pub fn main(machine:String,path:String)->loom::Value. Every root pub fn is an entry; keep traversal helpers private. Guest code has no macros or effect declarations; the driver infers effects. loom::fs::list(&machine, &path) -> Result<Vec<loom::DirEntry>,String>. Construct loom::Value objects using maps and Value constructors; do not use macros. Use loom::scope with s.spawn(|| loom::fs::list(&machine, &path)) and child.join() for concurrent directory listings. Implement traversal in guest. No exec/std::fs/fs.snapshot. Correctness and fast full-tree scans matter.`);
const trace=join(work,'trace.jsonl');
console.log(`Fresh Codex logs: ${work}`);
await execute(args,{env:{...process.env,LOOM_CODEX_TOKEN:token},stdout:Bun.file(trace),stderr:Bun.file(join(work,'stderr.log'))});
await execute(['bun',script('codex-mcp-check.ts'),'--trace',trace,'--fixture',fixture],{env:{...process.env,LOOM_URL:endpoint,LOOM_TOKEN:token}});

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
for(const tool of ['loom_define','loom_eval','loom_command','loom_resolve'])args.push('-c',`mcp_servers.loom.tools.${tool}.approval_mode="approve"`);
args.push(`Use only Loom MCP tools, no shell/web/other tools. Create machine with loom_command machine.create args {root:${JSON.stringify(fixture)}}. Write two recursive largest regular-file definitions named codex-recursive-ts and codex-recursive-rust with lang ts/rust. Both accept (machine:string,path:string) and return {path:string,size:number}, path relative to supplied root without ./ prefix. Skip symlinks and special files. Traverse every nested directory; ties choose lexicographically smallest path. Call both via loom_command call {hash,args:[machineId,"."]} and verify agreement. Retry compiler diagnostics until both work. Final JSON {ts_hash,rust_hash}.
TS imports from "loom": fs.list({machine,path}) returns Value entries {name,size,is_dir,is_file,is_symlink}. Export main, strict types, no any/async/nativeIO. Batch frontier using all(fs.list.desc(...)). Rust single-file #[loom::def(effects=["all", "fs.list"])] pub fn main(machine:String,path:String)->loom::Value. loom::abilities::fs::list(loom::Value::String(machine.clone()), &path) -> Result<loom::Value,String>. serde_json::json! available. Batch using loom::Desc::<loom::Value>::new("fs.list",serde_json::json!({"machine":machine,"path":path})) and loom::all(descs). Implement traversal in guest. No exec/std::fs/fs.snapshot. Correctness and fast full-tree scans matter.`);
const trace=join(work,'trace.jsonl');
console.log(`Fresh Codex logs: ${work}`);
await execute(args,{env:{...process.env,LOOM_CODEX_TOKEN:token},stdout:Bun.file(trace),stderr:Bun.file(join(work,'stderr.log'))});
await execute(['bun',script('codex-mcp-check.ts'),'--trace',trace,'--fixture',fixture],{env:{...process.env,LOOM_URL:endpoint,LOOM_TOKEN:token}});

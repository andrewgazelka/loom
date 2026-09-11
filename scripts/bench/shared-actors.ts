/** Production SDK actor controls for the shared-core backend. */
import {readFile} from 'node:fs/promises';
import {LoomMcpClient, object} from '../mcp-client';

function assert(value:unknown, message:string):asserts value {if(!value)throw new Error(message);}
const endpoint=process.env.LOOM_URL, tokenFile=process.env.LOOM_TOKEN_FILE;
assert(endpoint&&tokenFile,'Set LOOM_URL and LOOM_TOKEN_FILE for an isolated daemon');
const client=new LoomMcpClient({endpoint,token:(await readFile(tokenFile,'utf8')).trim()});
let passed=0;
const total=4;
let step='compile scoped actor and send';
async function command(command:string,args:unknown) {
  const reply=await client.callTool('loom_command',{command,args});
  assert(reply.ok,JSON.stringify(reply));
  return reply.result;
}
async function define(name:string,source:string) {
  const reply=await client.callTool('loom_define',{lang:'rust',name,source});
  assert(reply.ok,JSON.stringify(reply));
  const hash=object(object(reply.result).def).hash;
  assert(typeof hash==='string','definition hash missing');
  return hash;
}
const source=`
#[loom::actor(effects=["sleep"])]
pub struct Counter;
impl loom::Actor for Counter {
    type State=i64;
    type Event=i64;
    type Msg=i64;
    fn init()->i64 {0}
    fn fold(state:i64,event:&i64)->i64 {state+event}
    fn handle(state:&i64,msg:i64)->Vec<i64> {
        loom::scope(|scope| {
            let job=scope.spawn(|| {
                loom::sleep(1).expect("sleep");
                msg + *state
            }).expect("spawn child");
            vec![job.join().expect("child result")]
        })
    }
}`;
try {
  await client.connect();
  const hash=await define('shared-actor-gate',source);
  const actor=object(await command('spawn',{hash,initial:2})).id;
  assert(typeof actor==='string','actor id missing');
  assert(await command('send',{actor,msg:3})===7,'scoped handler/fold mismatch');
  passed++;
  step='fork actor and preserve independent history';
  const fork=object(await command('fork',{actor})).id;
  assert(typeof fork==='string','fork id missing');
  assert(await command('send',{actor:fork,msg:1})===15,'fork state mismatch');
  assert(await command('state',{actor})===7,'fork changed original actor');
  passed++;
  step='upgrade actor and replay existing events';
  const upgraded=await define('shared-actor-gate-v2',source.replace('state+event','state+event*2'));
  assert(await command('actor.upgrade',{actor,hash:upgraded})===12,'upgrade fold mismatch');
  passed++;
  step='deny effects during pure fold';
  const impure=await define('shared-actor-gate-impure',source.replace('state+event','{loom::sleep(1).expect("pure fold must reject effects"); state+event}'));
  const deniedActor=object(await command('spawn',{hash:impure,initial:0})).id;
  assert(typeof deniedActor==='string','denial actor missing');
  const denied=await client.callTool('loom_command',{command:'send',args:{actor:deniedActor,msg:1}});
  assert(!denied.ok&&/effects forbidden in pure core execution/.test(JSON.stringify(denied)),
    'expected specific pure-fold effect refusal');
  passed++;
} catch(error) {
  console.error(JSON.stringify({step,error:String(error)}));
} finally {
  await client.close();
  console.log(JSON.stringify({stage:'shared-actors',passed,total,first_failure:passed===total?null:step}));
  if(passed!==total)process.exitCode=1;
}

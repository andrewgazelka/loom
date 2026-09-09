import {test,expect} from 'bun:test';
import {Checker} from './checker';
const checker=new Checker();
test('strict checker rejects ambient IO and indexed undefined',()=>{
  expect(checker.check('export default function f(){return fetch("https://example.com")}').diagnostics.some(d=>d.code==='LOOM_CLOSED')).toBe(true);
  expect(checker.check('export default function f(xs:number[]):number{return xs[0]}').diagnostics.length).toBeGreaterThan(0);
});
test('canonical definition ignores whitespace and comments',()=>{
  const first=checker.check('export default function add(a:number,b:number):number {return a+b;}');
  const second=checker.check('// heading\nexport default function add( a: number, b: number ): number {\n return a + b;\n}');
  expect(first.diagnostics).toEqual([]);expect(first.canonical).toEqual(second.canonical);
});
test('closedness rejects dynamic import and constructors',()=>{
  expect(checker.check('export default function f(){return import("x")}').diagnostics.some(d=>d.code==='LOOM_CLOSED')).toBe(true);
  expect(checker.check('export default function f(){return (()=>1).constructor("return 2")()}').diagnostics.some(d=>d.code==='LOOM_CLOSED')).toBe(true);
});
test('warm checker p50 remains below target on small definitions',()=>{
  checker.check('export default function f(x:number){return x}');
  const durations:number[]=[];
  for(let i=0;i<20;i++){const start=performance.now();checker.check(`export default function f(x:number){return x+${i}}`);durations.push(performance.now()-start);}
  durations.sort((a,b)=>a-b);expect(durations[10]!).toBeLessThan(200);
});
test('canonical local renaming preserves shorthand keys and shadowed scopes',()=>{
  const left=checker.check('export default function f(x:number){const y=x+1; return y;}');
  const right=checker.check('export default function f(a:number){const b=a+1; return b;}');
  expect(left.canonical).toEqual(right.canonical);
  const shorthand=checker.check('export default function f(x:number){return {x};}');
  expect(shorthand.canonical).toContain('x: _local0');
  const destructured=checker.check('export default function f({x}:{x:number}){return x;}');
  expect(destructured.diagnostics).toEqual([]);
  expect(destructured.canonical).toContain('x: _local0');
});
test('Rust dependency signatures reject wrong TS arguments',()=>{
  const signatures={adder:{exports:[{name:'add',params:[{name:'a',shape:{type:'number' as const}},{name:'b',shape:{type:'number' as const}}],returns:{type:'number' as const},effects:{labels:[],unknown:false}}],effects:{labels:[],unknown:false}}};
  const accepted=checker.check('import {call} from "loom";import {adder} from "loom:defs";export default function f(){return call(adder,[20,22]);}',signatures);
  expect(accepted.diagnostics).toEqual([]);
  const rejected=checker.check('import {call} from "loom";import {adder} from "loom:defs";export default function f(){return call(adder,["wrong",22]);}',signatures);
  expect(rejected.diagnostics.length).toBeGreaterThan(0);
});
test('custom standard library removes ambient randomness through aliases',()=>{
  expect(checker.check('export default function f(){const math=Math;return math.random();}').diagnostics.length).toBeGreaterThan(0);
  expect(checker.check('import * as loom from "loom";export default function f(){return loom.random();}').diagnostics).toEqual([]);
});
test('canonical local renaming preserves export aliases',()=>{
  const result=checker.check('const worker=(x:number):number=>x+1;export {worker as default};');
  expect(result.diagnostics).toEqual([]);
  expect(checker.check(result.canonical).diagnostics).toEqual([]);
  expect(result.canonical).toContain('_local0 as default');
});
test('effect inference resolves aliases and shadows and propagates recursive helpers',()=>{
  const result=checker.check('import {exec as execute} from "loom";function a(n:number):number {if(n===0){execute(null);return 0;}return b(n-1);}function b(n:number):number{return a(n);}export function worker(){return b(2);}export function pure(){const execute=()=>3;return execute();}');
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.exports.find(item=>item.name==='worker')?.effects).toEqual({labels:['exec'],unknown:false});
  expect(result.sig.exports.find(item=>item.name==='pure')?.effects).toEqual({labels:[],unknown:false});
});
test('descriptors are inert until performed and unknown calls stay dynamic',()=>{
  const result=checker.check('import {exec,perform,all,now} from "loom";export function inert(){return exec.desc(null);}export function effects(){all([exec.desc(null),now.desc()]);return perform({op:"fs.read",args:null});}export function unknown(f:()=>number){return f();}');
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.exports.find(item=>item.name==='inert')?.effects).toEqual({labels:[],unknown:false});
  expect(result.sig.exports.find(item=>item.name==='effects')?.effects).toEqual({labels:['all','exec','fs.read','now'],unknown:false});
  expect(result.sig.exports.find(item=>item.name==='unknown')?.effects).toEqual({labels:[],unknown:true});
});
test('export aliases retain effect inference and dynamic descriptors',()=>{
  const result=checker.check('import {perform} from "loom";const worker=(op:string)=>perform({op,args:null});export {worker as default};');
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.exports[0]?.effects).toEqual({labels:[],unknown:true});
});

test('cross language dependency effects include scheduling operations',()=>{
  const signatures={worker:{exports:[{name:'default',params:[],returns:{type:'number' as const},effects:{labels:['exec'],unknown:false}}],effects:{labels:['exec'],unknown:false}}};
  const result=checker.check('import {fork,join,call} from "loom";import {worker as dependency} from "loom:defs";export default function f(){join([fork(dependency,[])]);return call(dependency,[]);}',signatures);
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.exports[0]?.effects).toEqual({labels:['call','exec','fork','join'],unknown:false});
});
test('module initialization and getters are visible; mutable descriptors stay unknown',()=>{
  const initialized=checker.check('import {now,random} from "loom";const time=now();const object={get value(){return random();}};export default function f(){return time+object.value;}');
  expect(initialized.diagnostics).toEqual([]);
  expect(initialized.sig.exports[0]?.effects).toEqual({labels:['now','random'],unknown:false});
  const mutated=checker.check('import {perform} from "loom";export default function f(){const desc={op:"now",args:null};desc.op="random";return perform(desc);}');
  expect(mutated.sig.exports[0]?.effects.unknown).toBe(true);
});

test('definition effects union handler exports and module initialization',()=>{
  const result=checker.check('import {now,exec,random} from "loom";const time=now();export function run(){return exec(null);}export function fold(){return random()+time;}');
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.effects).toEqual({labels:['exec','now','random'],unknown:false});
  expect(checker.check('export const value=42;').sig.effects).toEqual({labels:[],unknown:true});
});
test('dependency aggregate effects take precedence over a single export',()=>{
  const signatures={worker:{exports:[{name:'default',params:[],returns:{type:'number' as const},effects:{labels:[],unknown:false}}],effects:{labels:['exec'],unknown:true}}};
  const result=checker.check('import {call} from "loom";import {worker} from "loom:defs";export default function f(){return call(worker,[]);}',signatures);
  expect(result.diagnostics).toEqual([]);
  expect(result.sig.effects).toEqual({labels:['call','exec'],unknown:true});
});

test('implicit getter and iterator dispatch never reports a pure signature',()=>{
  for(const body of [
    'const {value}=obj;return value;',
    'const copy={...obj};return copy.value;',
    'let value=0;({value}=obj);return value;',
    'const nested={item:obj};const {item:{value}}=nested;return value;',
  ]) {
    const result=checker.check(`import {random} from "loom";export default function main():number {const obj={get value(){return random();}};${body}}`);
    expect(result.diagnostics).toEqual([]);
    expect(result.sig.effects.unknown).toBe(true);
    expect(result.sig.exports[0]?.effects.unknown).toBe(true);
  }
  const spread=checker.check('export default function main(xs:number[]){return [...xs];}');
  expect(spread.diagnostics).toEqual([]);
  expect(spread.sig.effects.unknown).toBe(true);
});

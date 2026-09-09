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
  const signatures={adder:{exports:[{name:'add',params:[{name:'a',shape:{type:'number' as const}},{name:'b',shape:{type:'number' as const}}],returns:{type:'number' as const}}]}};
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

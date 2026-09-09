import { expect, test } from 'bun:test';
import { CID } from 'multiformats/cid';
import { create } from 'multiformats/hashes/digest';
import { encode, decode } from './codec';
const digest = new Uint8Array(32); digest.set([0x77,0xc1]);
export const goldenCid = CID.createV1(0x71,create(0x1e,digest)).toString();
const hex = (bytes: Uint8Array): string => Array.from(bytes,b=>b.toString(16).padStart(2,'0')).join('');
const bytes = (hex:string):Uint8Array => Uint8Array.from(hex.match(/../g) ?? [],b=>Number.parseInt(b,16));

test('references become CID tag42 links recursively',()=>{
  const value = {nested:[{$ref:goldenCid}],none:null};
  const encoded = encode(value);
  expect(hex(encode({$ref:goldenCid}))).toBe('d82a58250001711e2077c1'+'00'.repeat(30));
  expect(decode(encoded)).toEqual(value);
});
test('DAG maps sort UTF8 length then bytes and preserve prototype keys',()=>{
  expect(hex(encode({bbb:0,a:0,aa:0}))).toBe('a3616100626161006362626200');
  const value=JSON.parse('{"__proto__":1,"é":2,"a":3}');
  expect(decode(encode(value))).toEqual(value);
  expect(Object.getPrototypeOf(decode(encode(value)))).toBe(Object.prototype);
});
test('safe integral numbers use integers, other finite numbers float64',()=>{
  expect(hex(encode(1.0))).toBe('01');
  expect(hex(encode(1.5))).toBe('fb3ff8000000000000');
  expect(hex(encode(1e20))).toBe('fb4415af1d78b58c40');
  // Float64 integral data is valid on input even though JS erases its wire kind.
  expect(decode(bytes('fb3ff0000000000000'))).toBe(1);
  expect(decode(encode(Number.MAX_SAFE_INTEGER))).toBe(Number.MAX_SAFE_INTEGER);
});
test('encode rejects values outside JSON/Ref and ambiguous refs',()=>{
  for (const value of [undefined,NaN,Infinity,-Infinity,-0,1n,new Map([[1,2]]),new Uint8Array([1]),{$ref:'bad'},{$ref:goldenCid,extra:1},{nested:undefined},[undefined]]) {
    expect(()=>encode(value)).toThrow();
  }
  const cycle:Record<string,unknown>={};cycle.self=cycle;
  expect(()=>encode(cycle)).toThrow();
  expect(()=>encode('\ud800')).toThrow();
});
test('decode rejects noncanonical and invalid wire input',()=>{
  const rejected = [
    'a2616200616100', // unsorted map
    'a2616100616101', // duplicate key
    'a10100', // non-string map key
    '1801', // nonminimal integer
    '780161', // nonminimal string length
    'f93c00', // float16
    'fa3f800000', // float32
    'fb8000000000000000', // negative zero
    'fb7ff0000000000000', // infinity
    'fb7ff8000000000000', // NaN
    '1b0020000000000000', // unsafe integer wire type
    '9fff', // indefinite array
    'bf616101ff', // indefinite map
    'f7', // undefined
    'd82b00', // unknown tag
    'd82a4100', // malformed CID
    'd82a0100', // non-byte CID
    '0100', // trailing data
    '61ff', // invalid UTF8
    '4100', // bytes outside reference
    'a1642472656663626164', // plain reserved $ref map
    '', '81', // truncated
  ];
  for(const wire of rejected) expect(()=>decode(bytes(wire)),wire).toThrow();
});
test('ToolCompleted exact shared Rust golden vector',()=>{
  const value={t:'ToolCompleted',id:'a1',output:{$ref:goldenCid}};
  const expected='a361746d546f6f6c436f6d706c65746564626964626131666f7574707574d82a58250001711e2077c1'+'00'.repeat(30);
  expect(hex(encode(value))).toBe(expected);
  expect(decode(bytes(expected))).toEqual(value);
});
test('WIT cross-realm byte views decode without weakening input validation',async()=>{
  const {runInNewContext}=await import('node:vm');
  const foreign:Uint8Array=runInNewContext('new Uint8Array([0xa1,0x61,0x61,0x01])');
  expect(foreign instanceof Uint8Array).toBe(false);
  expect(ArrayBuffer.isView(foreign)).toBe(true);
  expect(decode(foreign)).toEqual({a:1});
  expect(()=>decode([1] as unknown as Uint8Array)).toThrow();
  expect(()=>decode(new Uint16Array([1]) as unknown as Uint8Array)).toThrow();
});

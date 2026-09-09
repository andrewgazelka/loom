import {test, expect} from 'bun:test';
import {encode, decode} from './codec';
import {handler} from './adapter';
import { CID } from 'multiformats/cid';
import { create } from 'multiformats/hashes/digest';
test('actor executes and folds DAG-CBOR events',()=>{
 const guest=handler({init:()=>0,run:(_state,msg)=>[msg],fold:(state,event)=>Number(state)+Number(event)});
 expect(decode(guest.run(encode(null),encode(7)))).toEqual([7]);
 expect(decode(guest.fold(encode(null),encode(7)))).toBe(7);
});
test('free function preserves nested CID references through shared codec',()=>{
 const guest=handler({default:value=>value});
 const cid=CID.createV1(0x71,create(0x1e,new Uint8Array(32))).toString();
 const value={reference:{$ref:cid},nested:[null,true,3]};
 expect(decode(guest.call(new Uint8Array(),encode([value])))).toEqual(value);
});
test('adapter rejects malformed wire and nonvalues from user code',()=>{
 const guest=handler({default:()=>undefined});
 expect(()=>guest.call(new Uint8Array(),encode([]))).toThrow();
 expect(()=>guest.call(new Uint8Array(),new Uint8Array([0x18,0x01]))).toThrow();
});
test('WIT result errors carry payload strings instead of trapping',()=>{
 const guest=handler({default:()=>undefined});
 try { guest.call(new Uint8Array(),encode([])); throw new Error('expected call failure'); }
 catch(error) { expect((error as {payload?:string}).payload).toBe('call.encode: Value must be JSON or a CID reference'); }
});

import {test, expect} from 'bun:test';
import {Encoder, Decoder} from 'cbor-x';
import {handler} from './adapter';
const encoder = new Encoder({useRecords:false,mapsAsObjects:true});
const decoder = new Decoder({mapsAsObjects:true});
test('actor executes and folds CBOR events',()=>{
 const guest=handler({init:()=>0,run:(_state,msg)=>[msg],fold:(state,event)=>Number(state)+Number(event)});
 expect(decoder.decode(guest.run(encoder.encode(null),encoder.encode(7)))).toEqual([7]);
 expect(decoder.decode(guest.fold(encoder.encode(null),encoder.encode(7)))).toBe(7);
});
test('free function values preserve references and objects without CBOR extension tags',()=>{
 const guest=handler({default:value=>value});
 const value={$ref:'abc',nested:[null,true,3]};
 expect(decoder.decode(guest.call(new Uint8Array(),encoder.encode([value])))).toEqual(value);
});

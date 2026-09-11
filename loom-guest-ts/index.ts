import {encode, decode} from "./codec";
// The component builder leaves this import for the component linker.
import { perform as hostPerform } from "loom:host/effects";
import type { JsonValue } from "./protocol.generated";
export type Value = JsonValue;
export type { EffectSet } from "./protocol.generated";
export type Ref<T = Value> = { readonly $ref: string; readonly __type?: T };
declare const defBrand: unique symbol;
export interface Def<A = Value, R = Value> { readonly hash: string; readonly [defBrand]: { readonly args: A; readonly result: R } }
export function perform<T = Value>(op: string, args: Value): T { return decode(hostPerform(encode({op, args}))) as T; }
function effect<A extends Value, R>(op: string): (args: A) => R {
  return (args: A): R => perform<R>(op, args);
}
export const exec = effect<Value, Value>("exec");
export const llm = effect<Value, Value>("llm");
export function now(): number { return perform<number>("now", null); }
export function random(): number { return perform<number>("random", null); }
export function sleep(ms: number): Value { return perform("sleep", {ms}); }
export const fs = {list: effect<Value, Value>("fs.list"), stat: effect<Value, Value>("fs.stat"), read: effect<Value, Value>("fs.read"), snapshot: effect<Value, Ref>("fs.snapshot")};
export const cas = {put: effect<Value, Ref>("cas.put"), get: effect<Value, Value>("cas.get")};
export function call<A extends Value,R>(def: Def<A,R>, args:NoInfer<A>): R { return perform("call", {def:def.hash,args}); }
export const actor = {
  send(actor:string,msg:Value):Value { return perform("actor.send", {actor,msg}); },
  spawn<A extends Value,R>(def:Def<A,R>,state:Value=null):Value { return perform("actor.spawn", {def:def.hash,state}); },
};

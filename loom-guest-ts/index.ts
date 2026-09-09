import {encode, decode} from "./codec";
// The component builder leaves this import for the component linker.
import { perform as hostPerform } from "loom:host/abilities";
import type { JsonValue, Desc as WireDesc } from "./protocol.generated";
export type Value = JsonValue;
export type { EffectSet } from "./protocol.generated";
export type Ref<T = Value> = { readonly $ref: string; readonly __type?: T };
export type Desc<T = Value> = WireDesc & { readonly __result?: T };
declare const defBrand: unique symbol;
export interface Def<A = Value, R = Value> { readonly hash: string; readonly [defBrand]: { readonly args: A; readonly result: R } }
export interface Fiber<R = Value> { readonly id: Value; readonly __result?: R }
export function perform<T = Value>(desc: Desc<T>): T { return decode(hostPerform(encode(desc))) as T; }
function ability<A extends Value, R>(op: string) {
  const desc = (args: A): Desc<R> => ({op, args});
  return Object.assign((args: A): R => perform(desc(args)), {desc});
}
export const exec = ability<Value, Value>("exec");
export const llm = ability<Value, Value>("llm");
export const now = Object.assign(() => perform<number>({op:"now",args:null}), {desc: (): Desc<number> => ({op:"now",args:null})});
export const random = Object.assign(() => perform<number>({op:"random",args:null}), {desc: (): Desc<number> => ({op:"random",args:null})});
export const sleep = Object.assign((ms: number) => perform({op:"sleep",args:{ms}}), {desc: (ms:number): Desc => ({op:"sleep",args:{ms}})});
export const fs = {list: ability<Value, Value>("fs.list"), stat: ability<Value, Value>("fs.stat"), read: ability<Value, Value>("fs.read"), snapshot: ability<Value, Ref>("fs.snapshot")};
export const cas = {put: ability<Value, Ref>("cas.put"), get: ability<Value, Value>("cas.get")};
export function all<T>(descs: Desc<T>[]): T[] { return perform({op:"all",args:{descs:descs as unknown as Value}}); }
export function race<T>(descs: Desc<T>[]): T { return perform({op:"race",args:{descs:descs as unknown as Value}}); }
export function fork<A extends Value, R>(def: Def<A,R>, args: NoInfer<A>): Fiber<R> { return {id:perform({op:"fork",args:{def:def.hash,args}})}; }
export function join<R>(fibers: Fiber<R>[]): R[] { return perform({op:"join",args:{fibers:fibers.map(fiber=>fiber.id)}}); }
export function call<A extends Value,R>(def: Def<A,R>, args:NoInfer<A>): R { return perform({op:"call",args:{def:def.hash,args}}); }
export function send(actor:string,msg:Value): Value { return perform({op:"send",args:{actor,msg}}); }
export function spawn<A extends Value,R>(def:Def<A,R>,state:Value=null):Value { return perform({op:"spawn",args:{def:def.hash,state}}); }

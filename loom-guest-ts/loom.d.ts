declare module "loom" {
  export type Value = import("./protocol.generated").JsonValue;
  export type EffectSet = import("./protocol.generated").EffectSet;
  export interface Ref<T = Value> { readonly $ref: string; readonly __type?: T }
  const defBrand: unique symbol;
  export interface Def<A = Value, R = Value> { readonly hash: string; readonly [defBrand]: { readonly args: A; readonly result: R } }
  export function perform<T = Value>(op: string, args: Value): T;
  export type Effect<A, R> = (args: A) => R;
  export const exec: Effect<Value, Value>;
  export const llm: Effect<Value, Value>;
  export function now(): number;
  export function random(): number;
  export const sleep: Effect<number, Value>;
  export const fs: {list: Effect<Value,Value>;stat:Effect<Value,Value>;read:Effect<Value,Value>;snapshot:Effect<Value,Ref>};
  export const cas: {put:Effect<Value,Ref>;get:Effect<Value,Value>};
  export function call<A extends Value,R>(def:Def<A,R>,args:NoInfer<A>):R;
  export const actor: {
    send(actor:string,msg:Value):Value;
    spawn<A extends Value,R>(def:Def<A,R>,state?:Value):Value;
  };
}

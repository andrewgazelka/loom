declare module "loom" {
  export type Value = import("./protocol.generated").JsonValue;
  export interface Ref<T = Value> { readonly $ref: string; readonly __type?: T }
  export type Desc<T = Value> = import("./protocol.generated").Desc & { readonly __result?: T };
  const defBrand: unique symbol;
export interface Def<A = Value, R = Value> { readonly hash: string; readonly [defBrand]: { readonly args: A; readonly result: R } }
  export interface Fiber<R = Value> { readonly id: Value; readonly __result?: R }
  export function perform<T = Value>(desc: Desc<T>): T;
  export interface Ability<A, R> { (args: A): R; desc(args: A): Desc<R> }
  export const exec: Ability<Value, Value>;
  export const llm: Ability<Value, Value>;
  export const now: { (): number; desc(): Desc<number> };
  export const random: { (): number; desc(): Desc<number> };
  export const sleep: Ability<number, Value>;
  export const fs: {list: Ability<Value,Value>;stat:Ability<Value,Value>;read:Ability<Value,Value>;snapshot:Ability<Value,Ref>};
  export const cas: {put:Ability<Value,Ref>;get:Ability<Value,Value>};
  export function all<T>(descs: Desc<T>[]): T[];
  export function race<T>(descs: Desc<T>[]): T;
  export function fork<A extends Value,R>(def: Def<A,R>,args:NoInfer<A>):Fiber<R>;
  export function join<R>(fibers: Fiber<R>[]): R[];
  export function call<A extends Value,R>(def:Def<A,R>,args:NoInfer<A>):R;
  export function send(actor:string,msg:Value):Value;
  export function spawn<A extends Value,R>(def:Def<A,R>,state?:Value):Value;
}

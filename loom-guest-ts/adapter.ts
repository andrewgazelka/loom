import {encode, decode} from "./codec";
export interface GuestDefinition {
  run?: (state: unknown, msg: unknown) => unknown[];
  handle?: (state: unknown, msg: unknown) => unknown[];
  fold?: (state: unknown, event: unknown) => unknown;
  init?: () => unknown;
  default?: (...args: unknown[]) => unknown;
  main?: (...args: unknown[]) => unknown;
}
/** Componentize-JS lowers an error's payload into the WIT result error arm. */
class GuestError extends Error {
  readonly payload: string;
  constructor(stage: string, cause: unknown) {
    const detail = cause instanceof Error ? cause.message : String(cause);
    super(`${stage}: ${detail}`);
    this.payload = this.message;
  }
}
function boundary<T>(stage: string, action: () => T): T {
  try { return action(); } catch (error) { throw new GuestError(stage, error); }
}
export function handler(definition: GuestDefinition) {
  return {
    run(state: Uint8Array, msg: Uint8Array): Uint8Array {
      const fn = definition.run ?? definition.handle;
      if (!fn) throw new GuestError("run", "definition has no actor handler");
      const initial = boundary("run.decode-state", () => decode(state) ?? definition.init?.() ?? null);
      const message = boundary("run.decode-message", () => decode(msg));
      const events = boundary("run.handler", () => fn(initial, message));
      if (!Array.isArray(events)) throw new GuestError("run", "handler must return an event array");
      return boundary("run.encode", () => encode(events));
    },
    fold(state: Uint8Array, event: Uint8Array): Uint8Array {
      if (!definition.fold) throw new Error("definition has no fold");
      return encode(definition.fold(decode(state) ?? definition.init?.() ?? null, decode(event)));
    },
    call(_def: Uint8Array, args: Uint8Array): Uint8Array {
      const fn = definition.default ?? definition.main;
      if (!fn) throw new GuestError("call", "definition has no free function");
      const value: unknown = boundary("call.decode", () => decode(args));
      const result = boundary("call.function", () => fn(...(Array.isArray(value) ? value : value === null ? [] : [value])));
      return boundary("call.encode", () => encode(result));
    }
  };
}

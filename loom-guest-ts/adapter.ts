import { Encoder, Decoder } from "cbor-x";
const encoder = new Encoder({useRecords:false, mapsAsObjects:true});
const decoder = new Decoder({mapsAsObjects:true});
export interface GuestDefinition {
  run?: (state: unknown, msg: unknown) => unknown[];
  handle?: (state: unknown, msg: unknown) => unknown[];
  fold?: (state: unknown, event: unknown) => unknown;
  init?: () => unknown;
  default?: (...args: unknown[]) => unknown;
  main?: (...args: unknown[]) => unknown;
}
export function handler(definition: GuestDefinition) {
  return {
    run(state: Uint8Array, msg: Uint8Array): Uint8Array {
      const fn = definition.run ?? definition.handle;
      if (!fn) throw new Error("definition has no actor handler");
      const events = fn(decoder.decode(state) ?? definition.init?.() ?? null, decoder.decode(msg));
      if (!Array.isArray(events)) throw new Error("handler must return an event array");
      return encoder.encode(events);
    },
    fold(state: Uint8Array, event: Uint8Array): Uint8Array {
      if (!definition.fold) throw new Error("definition has no fold");
      return encoder.encode(definition.fold(decoder.decode(state) ?? definition.init?.() ?? null, decoder.decode(event)));
    },
    call(_def: Uint8Array, args: Uint8Array): Uint8Array {
      const fn = definition.default ?? definition.main;
      if (!fn) throw new Error("definition has no free function");
      const value: unknown = decoder.decode(args);
      return encoder.encode(fn(...(Array.isArray(value) ? value : value === null ? [] : [value])));
    }
  };
}

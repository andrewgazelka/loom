/** JSON/Ref boundary over the maintained DAG-CBOR codec. */
import { encode as encodeDag, decodeOptions } from '@ipld/dag-cbor';
import { decode as decodeCbor, Tokenizer, Type, type DecodeOptions } from 'cborg';
import { CID } from 'multiformats/cid';
import type { JsonValue } from './protocol.generated';

const utf8 = new TextEncoder();
const strictUtf8 = new TextDecoder('utf-8', {fatal: true});
const options: DecodeOptions = {
  ...decodeOptions,
  allowUndefined: false,
  coerceUndefinedToNull: false,
  allowBigInt: false,
  retainStringBytes: true,
};
const MAX_DEPTH = 256;
function checkDepth(depth: number): void {
  if (depth > MAX_DEPTH) throw new Error('DAG-CBOR nesting limit exceeded');
}
function checkString(value: string): void {
  if (strictUtf8.decode(utf8.encode(value)) !== value) throw new Error('Invalid Unicode string');
}
function toWire(value: unknown, depth: number, ancestors: Set<object>): unknown {
  checkDepth(depth);
  if (value === null || typeof value === 'boolean') return value;
  if (typeof value === 'string') { checkString(value); return value; }
  if (typeof value === 'number') {
    if (!Number.isFinite(value) || Object.is(value, -0)) throw new Error('Invalid DAG-CBOR number');
    return value;
  }
  if (typeof value !== 'object') throw new Error('Value must be JSON or a CID reference');
  if (ancestors.has(value)) throw new Error('Cyclic value');
  ancestors.add(value);
  try {
    if (Array.isArray(value)) {
      const result: unknown[] = [];
      for (let i = 0; i < value.length; i++) result.push(toWire(value[i], depth + 1, ancestors));
      return result;
    }
    if (Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null) {
      throw new Error('Value must be a plain object');
    }
    if (Object.getOwnPropertySymbols(value).length !== 0) throw new Error('Map keys must be strings');
    const object = value as Record<string, unknown>;
    const keys = Object.keys(object);
    if (Object.hasOwn(object, '$ref')) {
      const reference = Object.getOwnPropertyDescriptor(object, '$ref');
      if (Object.getOwnPropertyNames(object).length !== 1 || reference?.get || reference?.set || typeof reference?.value !== 'string') throw new Error('Reference must contain only a CID string $ref');
      return CID.parse(reference.value);
    }
    const result: Record<string, unknown> = Object.create(null);
    for (const key of keys) {
      checkString(key);
      const descriptor = Object.getOwnPropertyDescriptor(object, key);
      if (descriptor?.get || descriptor?.set) throw new Error('Object accessors are not values');
      result[key] = toWire(object[key], depth + 1, ancestors);
    }
    return result;
  } finally { ancestors.delete(value); }
}

/** The library reads all CBOR bytes. This token walk enforces DAG ordering and widths. */
function validateTokens(tokens: Tokenizer, depth = 0): void {
  checkDepth(depth);
  if (tokens.done()) throw new Error('Truncated DAG-CBOR item');
  const token = tokens.next();
  if (token.type === Type.float) {
    if (token.encodedLength !== 9 || Object.is(token.value, -0)) throw new Error('DAG-CBOR requires finite nonnegative-zero float64');
  } else if (token.type === Type.string) {
    if (token.byteValue) strictUtf8.decode(token.byteValue);
  } else if (token.type === Type.array) {
    for (let i = 0; i < token.value; i++) validateTokens(tokens, depth + 1);
  } else if (token.type === Type.map) {
    let previous: Uint8Array | undefined;
    for (let i = 0; i < token.value; i++) {
      if (tokens.done()) throw new Error('Truncated map');
      const key = tokens.next();
      if (key.type !== Type.string) throw new Error('Map keys must be strings');
      const bytes = key.byteValue ?? utf8.encode(key.value);
      strictUtf8.decode(bytes);
      if (previous && compareKeys(previous, bytes) >= 0) throw new Error('Map keys must be unique and sorted');
      previous = bytes;
      validateTokens(tokens, depth + 1);
    }
  } else if (token.type === Type.tag) {
    if (token.value !== 42 || tokens.done()) throw new Error('Only CID tag 42 is supported');
    const bytes = tokens.next();
    if (bytes.type !== Type.bytes || bytes.value[0] !== 0) throw new Error('CID tag requires bytes prefixed with zero');
    CID.decode(bytes.value.subarray(1));
  } else if (token.type === Type.bytes) {
    throw new Error('Byte strings require a CID tag; use a blob reference');
  }
}
function compareKeys(left: Uint8Array, right: Uint8Array): number {
  if (left.length !== right.length) return left.length - right.length;
  for (let i = 0; i < left.length; i++) {
    if (left[i] !== right[i]) return left[i]! - right[i]!;
  }
  return 0;
}
function fromWire(value: unknown, depth: number): JsonValue {
  checkDepth(depth);
  if (value === null || typeof value === 'boolean' || typeof value === 'string' || typeof value === 'number') return value;
  if (Array.isArray(value)) return value.map(item => fromWire(item, depth + 1));
  const cid = CID.asCID(value);
  if (cid) return {$ref: cid.toString()};
  if (typeof value !== 'object' || value === null) throw new Error('Decoded non-JSON value');
  if (Object.hasOwn(value, '$ref')) throw new Error('Wire references must use CID tag 42');
  const result: Record<string, JsonValue> = {};
  for (const [key, item] of Object.entries(value)) {
    Object.defineProperty(result, key, {value:fromWire(item,depth+1),enumerable:true,writable:true,configurable:true});
  }
  return result;
}
export function encode(value: unknown): Uint8Array {
  return encodeDag(toWire(value, 0, new Set()));
}
export function decode(input: Uint8Array): JsonValue {
  // StarlingMonkey's component binding allocates list<u8> in a separate realm.
  // ArrayBuffer.isView crosses realms; cborg's instanceof check intentionally does not.
  if (!ArrayBuffer.isView(input) || Object.prototype.toString.call(input) !== '[object Uint8Array]') {
    throw new Error('DAG-CBOR input must be a Uint8Array');
  }
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  const tokens = new Tokenizer(bytes, options);
  validateTokens(tokens);
  if (!tokens.done()) throw new Error('Trailing DAG-CBOR data');
  return fromWire(decodeCbor(bytes, options), 0);
}

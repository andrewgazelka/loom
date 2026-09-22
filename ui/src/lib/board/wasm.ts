/**
 * `GET /v1/wasm/{component_hash}`: the component as WebAssembly text, its functions, and the
 * DWARF line map from wat lines to source lines. Pure functions here; `WasmView.svelte` draws.
 */

/** The guest crate's own source file inside the build tree; every other file is external. */
export const OWN_FILE = "src/lib.rs";
/** The guest crate is materialized as package `loom-definition`, so its symbols start here. */
export const OWN_PREFIX = "loom_definition";

export interface WasmFunction {
  index: number;
  name: string | null;
  exported: boolean;
  /** 1-based, inclusive wat line range. */
  startLine: number;
  endLine: number;
}
export interface WasmLine {
  watLine: number;
  file: string;
  line: number;
}
export interface WasmModule {
  /** `false` when the artifact carries no DWARF: `lines` is then empty by construction. */
  debug: boolean;
  wat: string;
  functions: WasmFunction[];
  lines: WasmLine[];
  /**
   * The text the compiler saw: the stored source followed by the generated entry
   * wrappers, so own-file line numbers index into it. `null` with a reason in
   * `compiledSourceError` when the server could not reconstruct it.
   */
  compiledSource: string | null;
  compiledSourceError: string | null;
  /** 1-based line of `compiledSource` where the generated entry wrappers begin. */
  compiledWrapperLine: number | null;
}

function record(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    throw new Error(`${what}: expected an object`);
  return value as Record<string, unknown>;
}
function integer(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value))
    throw new Error(`${what}: expected an integer`);
  return value;
}
function text(value: unknown, what: string): string {
  if (typeof value !== "string") throw new Error(`${what}: expected a string`);
  return value;
}

/** Validate the wire shape; a protocol envelope or a missing field fails by name. */
export function parseWasmModule(value: unknown): WasmModule {
  const body = record(value, "wasm");
  if ("ok" in body && "result" in body)
    throw new Error("wasm: response is a command envelope, expected the bare module object");
  if (typeof body.debug !== "boolean") throw new Error("wasm.debug: expected a boolean");
  const wat = text(body.wat, "wasm.wat");
  if (!Array.isArray(body.functions)) throw new Error("wasm.functions: expected an array");
  if (!Array.isArray(body.lines)) throw new Error("wasm.lines: expected an array");
  const functions = body.functions.map((item, position): WasmFunction => {
    const fn = record(item, `wasm.functions[${position}]`);
    const name = fn.name === null ? null : text(fn.name, `wasm.functions[${position}].name`);
    if (typeof fn.exported !== "boolean")
      throw new Error(`wasm.functions[${position}].exported: expected a boolean`);
    const startLine = integer(fn.start_line, `wasm.functions[${position}].start_line`);
    const endLine = integer(fn.end_line, `wasm.functions[${position}].end_line`);
    if (startLine < 1 || endLine < startLine)
      throw new Error(`wasm.functions[${position}]: line range ${startLine}..${endLine} is empty`);
    return {
      index: integer(fn.index, `wasm.functions[${position}].index`),
      name,
      exported: fn.exported,
      startLine,
      endLine,
    };
  });
  const lines = body.lines.map((item, position): WasmLine => {
    const row = record(item, `wasm.lines[${position}]`);
    return {
      watLine: integer(row.wat_line, `wasm.lines[${position}].wat_line`),
      file: text(row.file, `wasm.lines[${position}].file`),
      line: integer(row.line, `wasm.lines[${position}].line`),
    };
  });
  const compiledSource =
    body.compiled_source === undefined || body.compiled_source === null
      ? null
      : text(body.compiled_source, "wasm.compiled_source");
  const compiledSourceError =
    body.compiled_source_error === undefined || body.compiled_source_error === null
      ? null
      : text(body.compiled_source_error, "wasm.compiled_source_error");
  const compiledWrapperLine =
    body.compiled_wrapper_line === undefined || body.compiled_wrapper_line === null
      ? null
      : integer(body.compiled_wrapper_line, "wasm.compiled_wrapper_line");
  return {
    debug: body.debug,
    wat,
    functions,
    lines,
    compiledSource,
    compiledSourceError,
    compiledWrapperLine,
  };
}

export interface LineIndex {
  byWat: Map<number, { file: string; line: number }>;
  /** `${file}\n${line}` to ascending wat lines. */
  bySource: Map<string, number[]>;
}
const sourceKey = (file: string, line: number) => `${file}\n${line}`;

export function indexLines(lines: WasmLine[]): LineIndex {
  const byWat = new Map<number, { file: string; line: number }>();
  const bySource = new Map<string, number[]>();
  for (const row of lines) {
    byWat.set(row.watLine, { file: row.file, line: row.line });
    const key = sourceKey(row.file, row.line);
    const list = bySource.get(key) ?? [];
    list.push(row.watLine);
    bySource.set(key, list);
  }
  for (const list of bySource.values()) list.sort((a, b) => a - b);
  return { byWat, bySource };
}

/** Every wat line the compiler attributed to `file:line`, ascending; empty when unmapped. */
export function watLinesFor(index: LineIndex, line: number, file: string = OWN_FILE): number[] {
  return index.bySource.get(sourceKey(file, line)) ?? [];
}

/** The source position of one wat line, or `null` when the line map does not cover it. */
export function sourceOf(index: LineIndex, watLine: number): { file: string; line: number } | null {
  return index.byWat.get(watLine) ?? null;
}

/**
 * A function is the definition's own when its name carries one of the export names or the
 * guest crate's symbol prefix. Everything else (std, alloc, core, the SDK, `cabi_realloc`) is
 * external and starts collapsed, exported or not.
 */
export function isOwnFunction(
  fn: Pick<WasmFunction, "name">,
  exports: string[],
  prefixes: string[] = [OWN_PREFIX],
): boolean {
  if (fn.name === null) return false;
  const name = fn.name;
  return (
    prefixes.some((prefix) => prefix.length > 0 && name.includes(prefix)) ||
    exports.some((exported) => exported.length > 0 && name.includes(exported))
  );
}

export interface WatGroup {
  key: string;
  label: string;
  start: number;
  end: number;
  own: boolean;
  fn: WasmFunction | null;
}

/** Each function label: the name, or `func #index` for an unnamed one. */
export function functionLabel(fn: WasmFunction): string {
  return fn.name ?? `func #${fn.index}`;
}

/**
 * Cover wat lines `1..lineCount` with one group per function plus `module` groups for the
 * lines between functions (types, imports, memory, data). Overlapping functions are a server
 * error and fail by index rather than being drawn twice.
 */
export function groupFunctions(
  functions: WasmFunction[],
  exports: string[],
  lineCount: number,
): WatGroup[] {
  const sorted = [...functions].sort((a, b) => a.startLine - b.startLine);
  const groups: WatGroup[] = [];
  let cursor = 1;
  for (const fn of sorted) {
    if (fn.startLine < cursor)
      throw new Error(
        `wasm.functions: function ${fn.index} starts at line ${fn.startLine}, inside the previous function`,
      );
    if (fn.startLine > cursor)
      groups.push({
        key: `module:${cursor}`,
        label: "module",
        start: cursor,
        end: fn.startLine - 1,
        own: false,
        fn: null,
      });
    groups.push({
      key: `fn:${fn.index}`,
      label: functionLabel(fn),
      start: fn.startLine,
      end: Math.min(fn.endLine, lineCount),
      own: isOwnFunction(fn, exports),
      fn,
    });
    cursor = fn.endLine + 1;
  }
  if (cursor <= lineCount)
    groups.push({
      key: `module:${cursor}`,
      label: "module",
      start: cursor,
      end: lineCount,
      own: false,
      fn: null,
    });
  return groups;
}

/** The group holding a wat line, for expanding a collapsed function when a mapping lands in it. */
export function groupOf(groups: WatGroup[], watLine: number): WatGroup | null {
  return groups.find((group) => group.start <= watLine && watLine <= group.end) ?? null;
}

/** The sentence shown when the artifact has no DWARF. */
export const NO_DEBUG_INFO = "This artifact carries no debug info; rebuild to map source lines.";

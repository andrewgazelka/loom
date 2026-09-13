import { V } from "./commands";
import type { Command } from "./commands";
import {
  actor,
  actorTree,
  array,
  definition,
  definitionDiff,
  definitionView,
  updateSession,
  history,
  integer,
  json,
  nullableString,
  object,
  rows,
  string,
  validation,
  type Json,
  type Row,
} from "./schema";
export interface Transport {
  request(command: Command, body: Row, signal?: AbortSignal): Promise<unknown>;
  activeBuild?(signal?: AbortSignal): Promise<unknown>;
  compilerLog?(hash: string, signal?: AbortSignal): Promise<string>;
}
export interface ActiveBuild {
  name: string;
  stage: "preflight" | "check" | "compile" | "publish";
  elapsed_ms: number;
}
export type FetchRequest = (
  input: string,
  init?: RequestInit,
) => Promise<Response>;
export class HttpTransport implements Transport {
  private base: string;
  constructor(
    endpoint: string,
    private token: string,
    private fetcher: FetchRequest = fetch,
  ) {
    this.base = endpoint.trim().replace(/\/$/, "");
    if (this.base) {
      const url = new URL(this.base);
      if (
        !["http:", "https:"].includes(url.protocol) ||
        url.username ||
        url.password ||
        url.search ||
        url.hash
      )
        throw new Error(
          "API endpoint: expected an HTTP(S) origin without credentials, query or fragment",
        );
    }
  }
  async request(
    command: Command,
    body: Row,
    signal?: AbortSignal,
  ): Promise<unknown> {
    const path = "/v1/command";
    const response = await this.fetcher(`${this.base}${path}`, {
      method: "POST",
      signal,
      headers: {
        "Content-Type": "application/json",
        ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
      },
      body: JSON.stringify({ command: command.operation, args: body }),
    });
    const text = await response.text();
    if (!response.ok)
      throw new Error(
        `${command.name}: HTTP ${response.status}: ${text.slice(0, 800)}`,
      );
    try {
      return JSON.parse(text);
    } catch {
      throw new Error(
        `${command.name}: HTTP ${response.status} returned invalid JSON`,
      );
    }
  }
  async activeBuild(signal?: AbortSignal): Promise<unknown> {
    const response = await this.fetcher(`${this.base}/v1/builds/active`, {
      signal,
      headers: this.token ? { Authorization: `Bearer ${this.token}` } : {},
    });
    if (!response.ok)
      throw new Error(`HTTP ${response.status}: ${await response.text()}`);
    return response.json();
  }
  async compilerLog(hash: string, signal?: AbortSignal): Promise<string> {
    if (!/^[a-f0-9]{64}$/.test(hash))
      throw new Error("Invalid compiler log hash");
    const response = await this.fetcher(`${this.base}/v1/cas/${hash}`, {
      signal,
      headers: this.token ? { Authorization: `Bearer ${this.token}` } : {},
    });
    if (!response.ok)
      throw new Error(`Compiler output: HTTP ${response.status}`);
    return response.text();
  }
}
/** Validate the result after unwrapping the shared protocol envelope. */
export function parseResult(command: Command, value: unknown): Json {
  const op = command.operation;
  if (command.group === "Definitions") {
    if (op === V.find) array(value, op).forEach(definition);
    else if (op === V.dependents)
      array(value, op).forEach((item) => string(item, `${op}[]`));
    else if (
      [
        V.update,
        V.update_view,
        V.update_repair,
        V.update_abort,
        V.update_rebase,
      ].some((verb) => verb === op)
    ) {
      updateSession(value);
      if (object(value, "update result").hash !== undefined)
        definitionView(value);
    } else if ([V.view, V.add].some((verb) => verb === op))
      definitionView(value);
    else if (op === V.history) history(value);
    else if (op === V.diff) definitionDiff(value);
    else if (op === V.run) {
      const data = object(value, op);
      json(data.output, "run.output");
      rows(data.effects, "run.effects");
    }
  } else if (op === V.tree) actorTree(value);
  else if (op === V.actors) array(value, op).forEach(actor);
  else if (op === V.info) {
    const info = object(value, op);
    string(info.status, "info.status");
    string(info.reason, "info.reason");
    string(info.behavior_hash, "info.behavior_hash");
    integer(info.cursor, "info.cursor");
    integer(info.inbox_len, "info.inbox_len");
    integer(info.deferred_len, "info.deferred_len");
    nullableString(info.parent, "info.parent");
    array(info.links, "info.links").forEach((value) => string(value, "link"));
    array(info.children, "info.children").forEach((value) =>
      string(value, "child"),
    );
    rows(info.monitors, "info.monitors");
  } else if (op === V.validate) validation(value);
  else if (
    [
      V.lineage,
      V.dead_letters,
      V.subscriptions,
      V.sql,
      V.behaviors,
      V.nodes,
    ].some((verb) => verb === op)
  )
    rows(value, op);
  else if (op === V.move) {
    const moved = object(value, op);
    string(moved.node_id, `${op}.node_id`);
    integer(moved.epoch, `${op}.epoch`);
  } else if ([V.members, V.promote_where].some((verb) => verb === op))
    array(value, op).forEach((value) => string(value, `${op}[]`));
  else if (op === V.whereis) nullableString(value, op);
  else if (op === V.send) {
    const sent = object(value, op);
    integer(sent.cursor, `${op}.cursor`);
    integer(sent.seq, `${op}.seq`);
    string(sent.id, `${op}.id`);
  } else if (op === V.drain)
    integer(object(value, op).processed, `${op}.processed`);
  else if (op === V.promote)
    string(object(value, op).behavior_hash, `${op}.behavior_hash`);
  else string(object(value, op).id, `${op}.id`);
  return json(value, command.name);
}
export function parseEnvelope(value: unknown): Json {
  const envelope = object(value, "response");
  if (typeof envelope.ok !== "boolean")
    throw new Error("response.ok: expected boolean");
  integer(envelope.seq, "response.seq");
  const diagnostics = rows(envelope.diagnostics, "response.diagnostics");
  for (const diagnostic of diagnostics) {
    for (const key of ["lang", "file", "code", "message"])
      string(diagnostic[key], `diagnostic.${key}`);
    integer(diagnostic.line, "diagnostic.line");
    integer(diagnostic.col, "diagnostic.col");
    nullableString(diagnostic.snippet, "diagnostic.snippet");
    nullableString(diagnostic.hint, "diagnostic.hint");
  }
  const result = json(envelope.result, "response.result");
  if (!envelope.ok) {
    const failure = object(result, "response.result");
    throw new Error(
      [
        string(failure.error, "response.result.error"),
        ...diagnostics.map((item) => item.message),
      ].join("\n"),
    );
  }
  return result;
}
export class WorkbenchClient {
  constructor(private transport: Transport) {}
  compilerLog(hash: string, signal?: AbortSignal): Promise<string> {
    if (!this.transport.compilerLog)
      throw new Error("Transport does not support compiler output");
    return this.transport.compilerLog(hash, signal);
  }
  async activeBuild(signal?: AbortSignal): Promise<ActiveBuild | null> {
    if (!this.transport.activeBuild)
      throw new Error("Transport does not support build progress");
    const result = object(
      parseEnvelope(await this.transport.activeBuild(signal)),
      "build progress",
    );
    if (result.active === null) return null;
    const active = object(result.active, "active build");
    const stage = string(active.stage, "build stage");
    if (
      stage !== "preflight" &&
      stage !== "check" &&
      stage !== "compile" &&
      stage !== "publish"
    )
      throw new Error(`Unknown build stage: ${stage}`);
    const elapsed_ms = integer(active.elapsed_ms, "build elapsed time");
    if (elapsed_ms < 0)
      throw new Error("Build elapsed time must be nonnegative");
    return { name: string(active.name, "build name"), stage, elapsed_ms };
  }
  async call(command: Command, body: Row, signal?: AbortSignal): Promise<Json> {
    try {
      return parseResult(
        command,
        parseEnvelope(await this.transport.request(command, body, signal)),
      );
    } catch (error) {
      throw new Error(
        `${command.name}: ${error instanceof Error ? error.message : error}`,
      );
    }
  }
}
/** A panel owns one slot. Superseded requests cannot replace its current result. */
export class RequestSlot {
  private generation = 0;
  private controller?: AbortController;
  cancel() {
    this.generation++;
    this.controller?.abort();
  }
  async run<T>(
    load: (signal: AbortSignal) => Promise<T>,
    receive: (value: T) => void,
    reject: (error: string) => void,
    finish: () => void,
  ) {
    this.cancel();
    const generation = this.generation;
    this.controller = new AbortController();
    try {
      const value = await load(this.controller.signal);
      if (generation === this.generation) receive(value);
    } catch (error) {
      if (generation === this.generation)
        reject(error instanceof Error ? error.message : String(error));
    } finally {
      if (generation === this.generation) finish();
    }
  }
}

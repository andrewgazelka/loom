import type { Command } from "./commands";
import {
  actor,
  actorTree,
  array,
  definition,
  definitionDiff,
  definitionView,
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
    const path = `/api/${command.group === "Actors" ? "actors" : "definitions"}/${command.operation}`;
    const response = await this.fetcher(`${this.base}${path}`, {
      method: "POST",
      signal,
      headers: {
        "Content-Type": "application/json",
        ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
      },
      body: JSON.stringify(body),
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
}
/** Validate both transports at the same boundary; HTTP returns raw MCP JSON, not a text envelope. */
export function parseResult(command: Command, value: unknown): Json {
  const op = command.operation;
  if (command.group === "Definitions") {
    if (["find", "dependents"].includes(op))
      array(value, op).forEach(definition);
    else if (["view", "add", "update"].includes(op)) definitionView(value);
    else if (op === "history") history(value);
    else if (op === "diff") definitionDiff(value);
    else if (op === "run") {
      const data = object(value, op);
      json(data.output, "run.output");
      rows(data.effects, "run.effects");
    }
  } else if (op === "actor_tree") actorTree(value);
  else if (op === "actor_list") array(value, op).forEach(actor);
  else if (op === "actor_info") {
    const info = object(value, op);
    string(info.status, "actor_info.status");
    string(info.reason, "actor_info.reason");
    string(info.behavior_hash, "actor_info.behavior_hash");
    integer(info.cursor, "actor_info.cursor");
    integer(info.inbox_len, "actor_info.inbox_len");
    integer(info.deferred_len, "actor_info.deferred_len");
    nullableString(info.parent, "actor_info.parent");
    array(info.links, "actor_info.links").forEach((value) =>
      string(value, "link"),
    );
    array(info.children, "actor_info.children").forEach((value) =>
      string(value, "child"),
    );
    rows(info.monitors, "actor_info.monitors");
  } else if (op === "actor_validate") validation(value);
  else if (
    [
      "actor_lineage",
      "actor_dead_letters",
      "actor_sql",
      "actor_behaviors",
    ].includes(op)
  )
    rows(value, op);
  else if (["actor_members", "actor_promote_where"].includes(op))
    array(value, op).forEach((value) => string(value, `${op}[]`));
  else if (op === "actor_whereis") nullableString(value, op);
  else if (op === "actor_send")
    integer(object(value, op).cursor, `${op}.cursor`);
  else if (op === "actor_run")
    integer(object(value, op).processed, `${op}.processed`);
  else if (op === "actor_promote")
    string(object(value, op).behavior_hash, `${op}.behavior_hash`);
  else string(object(value, op).id, `${op}.id`);
  return json(value, command.name);
}
export class WorkbenchClient {
  constructor(private transport: Transport) {}
  async call(command: Command, body: Row, signal?: AbortSignal): Promise<Json> {
    try {
      return parseResult(
        command,
        await this.transport.request(command, body, signal),
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

// Wire vocabulary mirrors crates/loom-proto/src/verbs.rs; consumers use V.
const verbNames = [
  "add",
  "view",
  "update",
  "history",
  "diff",
  "run",
  "find",
  "dependents",
  "spawn",
  "send",
  "tree",
  "info",
  "lineage",
  "validate",
  "promote",
  "fork",
  "actors",
  "stop",
  "restart",
  "dead_letters",
  "sql",
  "whereis",
  "register",
  "members",
  "behaviors",
  "promote_where",
  "drain",
] as const;
export type Verb = (typeof verbNames)[number];
export const V = Object.fromEntries(verbNames.map((name) => [name, name])) as {
  [Name in Verb]: Name;
};
export const panels = {
  inbox: "inbox",
  outbox: "outbox",
  effects: "effects",
} as const;
import { array, json, object, type Row } from "./schema";
export type Group = "Definitions" | "Actors";
export interface Field {
  key: string;
  label: string;
  kind?: "json" | "source" | "number";
  optional?: boolean;
  initial?: string;
  default?: string;
  options?: string[];
}
export interface Command {
  id: string;
  name: string;
  group: Group;
  operation: Verb;
  description: string;
  fields: Field[];
  read: boolean;
  query?: string;
}
const field = (
  key: string,
  label = key,
  extra: Partial<Field> = {},
): Field => ({ key, label, ...extra });
const id = field("id", "Actor id");
const hash = field("hash", "Definition hash");
const behavior = field("hash", "Behavior hash");
const author = field("author", "Author");
const rationale = field("rationale", "Rationale");
const source = field("source", "Rust source", { kind: "source" });
const definitionCommands: Command[] = [
  {
    id: V.find,
    name: V.find,
    group: "Definitions",
    operation: V.find,
    description: "Find definitions by name or hash",
    read: true,
    fields: [field("text", "Name or hash", { initial: "" })],
  },
  {
    id: V.view,
    name: V.view,
    group: "Definitions",
    operation: V.view,
    description: "Source, item identities and inferred entry effects",
    read: true,
    fields: [field("target", "Name or hash")],
  },
  {
    id: V.add,
    name: V.add,
    group: "Definitions",
    operation: V.add,
    description: "Add a Rust definition",
    read: false,
    fields: [
      field("name", "Name", { optional: true }),
      source,
      field("deps", "Dependency names → hashes", {
        kind: "json",
        optional: true,
      }),
      field("allowed_effects", "Allowed effects JSON", {
        kind: "json",
        optional: true,
      }),
    ],
  },
  {
    id: V.update,
    name: V.update,
    group: "Definitions",
    operation: V.update,
    description: "Update a named definition",
    read: false,
    fields: [
      field("name", "Name"),
      source,
      field("deps", "Dependency names → hashes", {
        kind: "json",
        optional: true,
      }),
      field("allowed_effects", "Allowed effects JSON", {
        kind: "json",
        optional: true,
      }),
    ],
  },
  {
    id: V.history,
    name: V.history,
    group: "Definitions",
    operation: V.history,
    description: "Hash chain and changed items",
    read: true,
    fields: [field("name", "Name")],
  },
  {
    id: V.diff,
    name: V.diff,
    group: "Definitions",
    operation: V.diff,
    description: "Compare two immutable definition hashes",
    read: true,
    fields: [
      field("old", "Before name or hash"),
      field("new", "After name or hash"),
    ],
  },
  {
    id: V.run,
    name: V.run,
    group: "Definitions",
    operation: V.run,
    description: "Run a definition and inspect performed effects",
    read: false,
    fields: [
      field("target", "Name or hash"),
      field("args", "Arguments JSON", {
        kind: "json",
        initial: "[]",
        default: "[]",
      }),
    ],
  },
  {
    id: V.dependents,
    name: V.dependents,
    group: "Definitions",
    operation: V.dependents,
    description: "Definitions that depend on this hash",
    read: true,
    fields: [hash],
  },
];
const actorCommand = (
  operation: Verb,
  description: string,
  fields: Field[],
  read: boolean,
): Command => ({
  id: operation,
  name: operation,
  group: "Actors",
  operation,
  description,
  fields,
  read,
});
export const commands: Command[] = [
  ...definitionCommands,
  actorCommand(
    V.tree,
    "Supervision tree from the root down",
    [field("root", "Root actor id", { optional: true })],
    true,
  ),
  actorCommand(V.actors, "Every actor in this node, including forks", [], true),
  actorCommand(V.info, "Lifecycle, cursor and relationships", [id], true),
  actorCommand(
    V.lineage,
    "Behavior history and promotion rationale",
    [id],
    true,
  ),
  actorCommand(V.dead_letters, "Failed messages and their errors", [id], true),
  actorCommand(
    V.validate,
    "Replay a candidate against recorded history",
    [
      id,
      field("candidate", "Candidate hash"),
      field("k", "Replay window k", { kind: "number", initial: "2" }),
      field("assertions", "SQL assertions JSON", {
        kind: "json",
        optional: true,
      }),
    ],
    false,
  ),
  actorCommand(
    V.promote,
    "Promote behavior for subsequent messages",
    [id, behavior, author, rationale],
    false,
  ),
  actorCommand(
    V.spawn,
    "Spawn a behavior under a supervisor",
    [
      field("def", "Behavior name or hash"),
      field("init", "Initialization JSON", {
        kind: "json",
        initial: "null",
        default: "null",
      }),
      field("parent", "Parent actor id", { optional: true }),
      field("spec", "Child spec JSON", { kind: "json", optional: true }),
    ],
    false,
  ),
  actorCommand(
    V.send,
    "Send a keyed message and run until idle",
    [
      id,
      field("key", "Delivery key", { optional: true }),
      field("msg", "Message JSON", { kind: "json", initial: "{}" }),
    ],
    false,
  ),
  actorCommand(
    V.stop,
    "Stop with a supplied reason",
    [id, field("reason", "Reason")],
    false,
  ),
  actorCommand(
    V.restart,
    "Resume, skip or reset an actor",
    [
      id,
      field("verb", "Restart verb", {
        options: ["resume", "skip", "reset"],
        initial: "resume",
      }),
    ],
    false,
  ),
  actorCommand(
    V.promote_where,
    "Promote all actors using a behavior",
    [
      field("old", "Old behavior hash"),
      field("new", "New behavior hash"),
      author,
      rationale,
    ],
    false,
  ),
  actorCommand(
    V.fork,
    "Create an undeliverable historical fork",
    [id, field("seq", "Sequence", { kind: "number" })],
    false,
  ),
  actorCommand(
    V.sql,
    "Read-only SQL on the actor database",
    [
      id,
      field("query", "SQL query", { kind: "source" }),
      field("params", "Parameters JSON", { kind: "json", optional: true }),
    ],
    true,
  ),
  actorCommand(
    V.whereis,
    "Resolve a registered name",
    [field("name", "Registered name")],
    true,
  ),
  actorCommand(
    V.register,
    "Register a unique actor name",
    [field("name", "Registered name"), id],
    false,
  ),
  actorCommand(
    V.members,
    "List actors in a group",
    [field("group", "Group")],
    true,
  ),
  actorCommand(V.behaviors, "Registered behaviors and descriptions", [], true),
  actorCommand(V.drain, "Run the node until idle", [], false),
  ...Object.values(panels).map((table) => ({
    ...actorCommand(V.sql, `Read the ${table} table`, [id], true),
    id: table,
    name: `${V.sql} ${table}`,
    query: `SELECT * FROM ${table} ORDER BY seq${table === "inbox" ? "" : ",idx"}`,
  })),
];
export function commandById(id: string): Command {
  const command = commands.find((command) => command.id === id);
  if (!command) throw new Error(`Unknown command: ${id}`);
  return command;
}
export function parseFields(
  command: Command,
  values: Record<string, string>,
): Row {
  const result: Row = {};
  for (const field of command.fields) {
    let value = values[field.key] ?? field.default ?? "";
    if (!value.trim() && field.default !== undefined) value = field.default;
    if (!value.trim() && field.optional) continue;
    if (!value.trim() && !(command.id === V.find && field.key === "text"))
      throw new Error(`${command.name}: ${field.label} is required`);
    if (field.kind === "json") {
      try {
        result[field.key] = json(JSON.parse(value), field.label);
      } catch (error) {
        throw new Error(
          `${command.name}: ${field.label}: ${error instanceof Error ? error.message : error}`,
        );
      }
    } else if (field.kind === "number") {
      const number = Number(value);
      if (!/^-?\d+$/.test(value.trim()) || !Number.isSafeInteger(number))
        throw new Error(`${field.label}: expected a safe integer`);
      if (field.key === "k" && (number < 0 || number > 4294967295))
        throw new Error(
          `${field.label}: expected a nonnegative count at most 4294967295`,
        );
      result[field.key] = number;
    } else result[field.key] = field.kind === "source" ? value : value.trim();
  }
  if (
    "assertions" in result &&
    result.assertions !== null &&
    array(result.assertions, "assertions").some(
      (value) => typeof value !== "string",
    )
  )
    throw new Error("assertions: expected SQL strings");
  if ("args" in result) array(result.args, "args");
  if (
    "params" in result &&
    result.params !== null &&
    array(result.params, "params").some(
      (value) => value !== null && typeof value === "object",
    )
  )
    throw new Error("params: expected scalar SQL values");
  if (
    "deps" in result &&
    Object.values(object(result.deps, "deps")).some(
      (value) => typeof value !== "string",
    )
  )
    throw new Error("deps: expected dependency hashes");
  if (
    "allowed_effects" in result &&
    result.allowed_effects !== null &&
    array(result.allowed_effects, "allowed_effects").some(
      (value) => typeof value !== "string",
    )
  )
    throw new Error("allowed_effects: expected effect labels");
  if ("spec" in result && result.spec !== null) object(result.spec, "spec");
  if (command.query) result.query = command.query;
  return result;
}

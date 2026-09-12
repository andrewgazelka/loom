import { array, json, object, type Row } from "./schema";
export type Group = "Definitions" | "Actors";
export interface Field {
  key: string;
  label: string;
  kind?: "json" | "source" | "number";
  optional?: boolean;
  initial?: string;
  options?: string[];
}
export interface Command {
  id: string;
  name: string;
  group: Group;
  operation: string;
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
const behavior = field("behavior_hash", "Behavior hash");
const author = field("author", "Author");
const rationale = field("rationale", "Rationale");
const source = field("source", "Rust source", { kind: "source" });
const definitionCommands: Command[] = [
  {
    id: "find",
    name: "find",
    group: "Definitions",
    operation: "find",
    description: "Find definitions by name or hash",
    read: true,
    fields: [field("query", "Name or hash", { initial: "" })],
  },
  {
    id: "view",
    name: "view",
    group: "Definitions",
    operation: "view",
    description: "Source, item identities and preimage sizes",
    read: true,
    fields: [hash],
  },
  {
    id: "add",
    name: "add",
    group: "Definitions",
    operation: "add",
    description: "Add a Rust definition",
    read: false,
    fields: [
      field("name", "Name"),
      source,
      field("deps", "Dependency names → hashes", {
        kind: "json",
        initial: "{}",
      }),
    ],
  },
  {
    id: "update",
    name: "update",
    group: "Definitions",
    operation: "update",
    description: "Update a named definition against its current hash",
    read: false,
    fields: [
      field("name", "Name"),
      field("expected_hash", "Current hash"),
      source,
      field("deps", "Dependency names → hashes (blank preserves current)", {
        kind: "json",
        optional: true,
      }),
    ],
  },
  {
    id: "history",
    name: "history",
    group: "Definitions",
    operation: "history",
    description: "Hash chain and changed items",
    read: true,
    fields: [field("name", "Name")],
  },
  {
    id: "diff",
    name: "diff",
    group: "Definitions",
    operation: "diff",
    description: "Compare two immutable definition hashes",
    read: true,
    fields: [field("before", "Before hash"), field("after", "After hash")],
  },
  {
    id: "run",
    name: "run",
    group: "Definitions",
    operation: "run",
    description: "Run a definition and inspect performed effects",
    read: false,
    fields: [
      hash,
      field("args", "Arguments JSON", { kind: "json", initial: "[]" }),
    ],
  },
  {
    id: "dependents",
    name: "dependents",
    group: "Definitions",
    operation: "dependents",
    description: "Definitions that depend on this hash",
    read: true,
    fields: [hash],
  },
];
const actorCommand = (
  operation: string,
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
    "actor_tree",
    "Supervision tree from the root down",
    [field("root", "Root actor id", { optional: true })],
    true,
  ),
  actorCommand(
    "actor_list",
    "Every actor in this node, including forks",
    [],
    true,
  ),
  actorCommand("actor_info", "Lifecycle, cursor and relationships", [id], true),
  actorCommand(
    "actor_lineage",
    "Behavior history and promotion rationale",
    [id],
    true,
  ),
  actorCommand(
    "actor_dead_letters",
    "Failed messages and their errors",
    [id],
    true,
  ),
  actorCommand(
    "actor_validate",
    "Replay a candidate against recorded history",
    [
      id,
      field("candidate_hash", "Candidate hash"),
      field("k", "Replay window k", { kind: "number", initial: "2" }),
      field("assertions", "SQL assertions JSON", {
        kind: "json",
        initial: "[]",
      }),
    ],
    false,
  ),
  actorCommand(
    "actor_promote",
    "Promote behavior for subsequent messages",
    [id, behavior, author, rationale],
    false,
  ),
  actorCommand(
    "actor_spawn",
    "Spawn a behavior under a supervisor",
    [
      behavior,
      field("init", "Initialization JSON", { kind: "json", initial: "null" }),
      field("parent", "Parent actor id", { optional: true }),
      field("spec", "Child spec JSON", { kind: "json", optional: true }),
    ],
    false,
  ),
  actorCommand(
    "actor_send",
    "Send a keyed message and run until idle",
    [
      id,
      field("key", "Delivery key", { optional: true }),
      field("msg", "Message JSON", { kind: "json", initial: "{}" }),
    ],
    false,
  ),
  actorCommand(
    "actor_stop",
    "Stop with a supplied reason",
    [id, field("reason", "Reason")],
    false,
  ),
  actorCommand(
    "actor_restart",
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
    "actor_promote_where",
    "Promote all actors using a behavior",
    [
      field("old_hash", "Old behavior hash"),
      field("new_hash", "New behavior hash"),
      author,
      rationale,
    ],
    false,
  ),
  actorCommand(
    "actor_fork",
    "Create an undeliverable historical fork",
    [id, field("at_seq", "Sequence", { kind: "number" })],
    false,
  ),
  actorCommand(
    "actor_sql",
    "Read-only SQL on the actor database",
    [
      id,
      field("query", "SQL query", { kind: "source" }),
      field("params", "Parameters JSON", { kind: "json", initial: "[]" }),
    ],
    true,
  ),
  actorCommand(
    "actor_whereis",
    "Resolve a registered name",
    [field("name", "Registered name")],
    true,
  ),
  actorCommand(
    "actor_register",
    "Register a unique actor name",
    [field("name", "Registered name"), id],
    false,
  ),
  actorCommand(
    "actor_members",
    "List actors in a group",
    [field("group", "Group")],
    true,
  ),
  actorCommand(
    "actor_behaviors",
    "Registered behaviors and descriptions",
    [],
    true,
  ),
  actorCommand("actor_run", "Run the node until idle", [], false),
  ...["inbox", "outbox", "effects"].map((table) => ({
    ...actorCommand("actor_sql", `Read the ${table} table`, [id], true),
    id: `actor_${table}`,
    name: `actor_sql ${table}`,
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
    const value = values[field.key] ?? "";
    if (!value.trim() && field.optional) continue;
    if (!value.trim() && !(command.id === "find" && field.key === "query"))
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
      if (!/^\d+$/.test(value.trim()) || !Number.isSafeInteger(number))
        throw new Error(`${field.label}: expected a nonnegative safe integer`);
      result[field.key] = number;
    } else result[field.key] = field.kind === "source" ? value : value.trim();
  }
  if (
    "assertions" in result &&
    array(result.assertions, "assertions").some(
      (value) => typeof value !== "string",
    )
  )
    throw new Error("assertions: expected SQL strings");
  if ("args" in result) array(result.args, "args");
  if (
    "params" in result &&
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
  if ("spec" in result) object(result.spec, "spec");
  if (command.query) result.query = command.query;
  return result;
}

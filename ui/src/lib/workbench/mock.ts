import fixture from "./fixtures.json";
import type { Command } from "./commands";
import type { Transport } from "./client";
import { type Row } from "./schema";
export const fixtures = fixture;
export class MockTransport implements Transport {
  constructor(private verdict = "DivergedAt") {
    if (!Object.hasOwn(fixture.verdicts, verdict))
      throw new Error(`Mock verdict: unknown ${verdict}`);
  }
  async request(command: Command, body: Row): Promise<unknown> {
    const op = command.operation;
    if (command.group === "Definitions") {
      if (op === "find")
        return fixture.definitions.filter((def) =>
          `${def.name} ${def.hash}`.includes(String(body.query ?? "")),
        );
      if (op === "dependents") return [fixture.definitions[1]];
      if (op === "history") return fixture.history;
      if (op === "diff") {
        const before = fixture.definitions.find(
          (def) => def.hash === body.before,
        );
        const after = fixture.definitions.find(
          (def) => def.hash === body.after,
        );
        if (!before || !after)
          throw new Error("Mock diff: select two fixture hashes");
        return {
          before: before.hash,
          after: after.hash,
          source_before: before.source,
          source_after: after.source,
          changed_items: fixture.history[0]!.changed_items,
        };
      }
      if (op === "run") return fixture.run;
      if (op === "view") {
        const def = fixture.definitions.find((def) => def.hash === body.hash);
        if (!def) throw new Error(`Mock view: unknown definition ${body.hash}`);
        return def;
      }
      if (op === "add" || op === "update") return fixture.definitions[0];
    }
    if (op === "actor_list") return fixture.actors;
    if (op === "actor_tree") {
      if (!body.root) return fixture.tree;
      const search = (node: typeof fixture.tree): unknown =>
        node.id === body.root
          ? node
          : node.children
              .map((child) => search(child as typeof fixture.tree))
              .find(Boolean);
      const root = search(fixture.tree);
      if (!root) throw new Error(`Mock actor_tree: unknown root ${body.root}`);
      return root;
    }
    if (op === "actor_info") {
      const actor = fixture.actors.find((actor) => actor.id === body.id);
      if (!actor) throw new Error(`Mock actor_info: unknown actor ${body.id}`);
      const { id, ...info } = actor;
      return {
        ...info,
        deferred_len: 0,
        reason: "",
        links: actor.parent ? [actor.parent] : [],
        monitors: [],
        children: fixture.actors
          .filter((child) => child.parent === actor.id)
          .map((child) => child.id),
      };
    }
    if (op === "actor_validate")
      return fixture.verdicts[this.verdict as keyof typeof fixture.verdicts];
    if (op === "actor_lineage") return fixture.lineage;
    if (op === "actor_dead_letters") return fixture.dead_letters;
    if (op === "actor_promote") return fixture.lineage[1];
    if (op === "actor_sql") {
      for (const table of ["inbox", "outbox", "effects"] as const) {
        if (
          body.query ===
          `SELECT * FROM ${table} ORDER BY seq${table === "inbox" ? "" : ",idx"}`
        )
          return fixture[table];
      }
      throw new Error(
        "Mock actor_sql: use an inbox, outbox or effects fixture query",
      );
    }
    if (Object.hasOwn(fixture.responses, op))
      return fixture.responses[op as keyof typeof fixture.responses];
    throw new Error(`Mock transport: no fixture for ${command.id}`);
  }
}

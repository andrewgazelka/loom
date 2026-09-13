import fixture from "./fixtures.json";
import { V, type Command } from "./commands";
import type { Transport } from "./client";
import { definition, type Row } from "./schema";
export const fixtures = fixture;
export class MockTransport implements Transport {
  private seq = 0;
  constructor(private verdict = "DivergedAt") {
    if (!Object.hasOwn(fixture.verdicts, verdict))
      throw new Error(`Mock verdict: unknown ${verdict}`);
  }
  async request(command: Command, body: Row): Promise<unknown> {
    const seq = ++this.seq;
    try {
      return {
        ...fixture.responseEnvelope,
        seq,
        result: this.result(command, body),
      };
    } catch (error) {
      return {
        ...fixture.responseEnvelope,
        ok: false,
        seq,
        result: {
          error: error instanceof Error ? error.message : String(error),
          code: "operation_failed",
        },
      };
    }
  }
  private result(command: Command, body: Row): unknown {
    const op = command.operation;
    if (op === V.find)
      return fixture.definitions
        .filter(
          (def) =>
            def.name.includes(String(body.text ?? "")) ||
            Object.keys(def.items).some((name) =>
              name.includes(String(body.text ?? "")),
            ),
        )
        .map((def) => ({
          name: def.name,
          hash: def.hash,
          items: Object.fromEntries(
            Object.entries(def.items).filter(([name]) =>
              name.includes(String(body.text ?? "")),
            ),
          ),
        }));
    if (op === V.dependents) return [fixture.definitions[1]!.hash];
    if (op === V.history) return fixture.history;
    if (op === V.diff) {
      const old = fixture.definitions.find(
        (def) => def.hash === body.old || def.name === body.old,
      );
      const next = fixture.definitions.find(
        (def) => def.hash === body.new || def.name === body.new,
      );
      if (!old || !next)
        throw new Error("Mock diff: select two fixture definitions");
      const before = definition(old).items,
        after = definition(next).items;
      return {
        old: old.hash,
        new: next.hash,
        added: Object.keys(after)
          .filter((name) => !(name in before))
          .map((name) => ({ name, hash: after[name] })),
        removed: Object.keys(before)
          .filter((name) => !(name in after))
          .map((name) => ({ name, hash: before[name] })),
        changed: Object.keys(before)
          .filter((name) => name in after && before[name] !== after[name])
          .map((name) => ({ name, old: before[name], new: after[name] })),
      };
    }
    if (op === V.run) return fixture.run;
    if (op === V.view) {
      const def = fixture.definitions.find(
        (def) => def.hash === body.target || def.name === body.target,
      );
      if (!def) throw new Error(`Mock view: unknown definition ${body.target}`);
      return def;
    }
    if (op === V.add || op === V.update)
      return { ...fixture.definitions[0], build: fixture.build };
    if (op === V.actors) return fixture.actors;
    if (op === V.tree) {
      if (!body.root) return fixture.tree;
      const search = (node: typeof fixture.tree): unknown =>
        node.id === body.root
          ? node
          : node.children
              .map((child) => search(child as typeof fixture.tree))
              .find(Boolean);
      const root = search(fixture.tree);
      if (!root) throw new Error(`Mock tree: unknown root ${body.root}`);
      return root;
    }
    if (op === V.info) {
      const actor = fixture.actors.find((actor) => actor.id === body.id);
      if (!actor) throw new Error(`Mock info: unknown actor ${body.id}`);
      const { id, ...info } = actor;
      return {
        ...info,
        deferred_len: 0,
        reason: "",
        links: actor.parent ? [actor.parent] : [],
        monitors: [],
        children: fixture.actors
          .filter((child) => child.parent === id)
          .map((child) => child.id),
      };
    }
    if (op === V.validate)
      return fixture.verdicts[this.verdict as keyof typeof fixture.verdicts];
    if (op === V.lineage) return fixture.lineage;
    if (op === V.dead_letters) return fixture.dead_letters;
    if (op === V.subscriptions) return fixture.subscriptions;
    if (op === V.nodes) return fixture.nodes;
    if (op === V.move) return fixture.responses.moved;
    if (op === V.promote) return fixture.lineage[1];
    if (op === V.sql) {
      for (const table of ["inbox", "outbox", "effects"] as const)
        if (
          body.query ===
          `SELECT * FROM ${table} ORDER BY seq${table === "inbox" ? "" : ",idx"}`
        )
          return fixture[table];
      throw new Error(
        "Mock SQL: use an inbox, outbox or effects fixture query",
      );
    }
    const responses = {
      [V.send]: fixture.responses.messageSent,
      [V.spawn]: fixture.responses.spawned,
      [V.stop]: fixture.responses.stopped,
      [V.restart]: fixture.responses.restarted,
      [V.promote_where]: fixture.responses.promotedActors,
      [V.fork]: fixture.responses.forked,
      [V.whereis]: fixture.responses.registeredActor,
      [V.register]: fixture.responses.registration,
      [V.members]: fixture.responses.groupMembers,
      [V.behaviors]: fixture.responses.knownBehaviors,
      [V.drain]: fixture.responses.drained,
    };
    if (Object.hasOwn(responses, op))
      return responses[op as keyof typeof responses];
    throw new Error(`Mock transport: no fixture for ${command.id}`);
  }
}

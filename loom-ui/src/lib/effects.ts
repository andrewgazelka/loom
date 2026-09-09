import { record, type LogEvent } from "./api";
export interface EffectRow { event: LogEvent; data: Record<string, unknown>; status: string; }
export function effectRows(events: LogEvent[]): EffectRow[] {
  const pending = new Map<string, EffectRow[]>();
  const orphanRecords = new Map<string, EffectRow[]>();
  const hidden = new Set<EffectRow>();
  const rows: EffectRow[] = [];
  const key = (data: Record<string, unknown>) => JSON.stringify({ scope: data.scope, occurrence: data.occurrence, desc_hash: data.desc_hash });
  for (const event of [...events].sort((left, right) => left.seq - right.seq)) {
    const data = record(event.event), identity = key(data);
    if (data.type === "effect_invoked") {
      const row = { event, data, status: "Invoked" };
      const queue = pending.get(identity) ?? [];
      queue.push(row);
      pending.set(identity, queue);
      rows.push(row);
    } else if (data.type === "effect_completed") {
      const status = data.error ? "Failed" : data.cached ? "Cached" : "Completed";
      const row = pending.get(identity)?.shift();
      if (row) { row.data = { ...row.data, ...data }; row.status = status; }
      else {
        const recorded = orphanRecords.get(identity)?.shift();
        if (recorded) hidden.add(recorded);
        rows.push({ event, data, status });
      }
    } else if (data.type === "effect_denied") {
      rows.push({ event, data, status: "Denied" });
    } else if (data.type === "effect_recorded" && !pending.get(identity)?.length) {
      const row = { event, data, status: "Recorded" };
      const queue = orphanRecords.get(identity) ?? [];
      queue.push(row);
      orphanRecords.set(identity, queue);
      rows.push(row);
    }
  }
  return rows.filter(row => !hidden.has(row));
}

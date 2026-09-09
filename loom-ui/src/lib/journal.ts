import { record, type Reply, type LogEvent, type Definition } from "./api";
export interface Entry {
  id: number;
  source: string;
  mode: string;
  name?: string;
  reply?: Reply;
  error?: string;
  ms?: number;
}
export interface JournalRow {
  id: string;
  kind: string;
  title: string;
  language: string;
  source?: string;
  value?: unknown;
  metadata?: unknown;
  seq?: number;
  entry?: Entry;
  timestamp?: number;
  sourceRef?: string;
  occurrences?: JournalRow[];
}
export function reference(value: unknown): string {
  return typeof value === "string"
    ? value
    : typeof record(value).$ref === "string"
      ? String(record(value).$ref)
      : "";
}
export function definitionName(definition: Definition): string {
  return (
    definition.name || definition.name_hint || definition.hash.slice(0, 16)
  );
}
export function eventRow(event: LogEvent): JournalRow {
  const data = record(event.event),
    definition = record(data.def),
    type = String(data.type || "event");
  const result: JournalRow = {
    id: `event-${event.seq}`,
    kind: type,
    title: type.replaceAll("_", " "),
    language: String(definition.lang || ""),
    metadata: event.event,
    seq: event.seq,
    timestamp: event.ts,
  };
  if (type === "defined") {
    result.title = String(data.name || "Definition");
    result.sourceRef = reference(data.source_hash);
    result.value = definition;
  } else if (type === "evaluated") {
    result.title = typeof data.source === "string" ? data.source : "Evaluation";
    result.source = typeof data.source === "string" ? data.source : undefined;
    result.language = "ts";
    result.value = data.result;
  } else if (type === "actor_created") {
    const actor = record(data.actor);
    result.title = `Actor created · ${String(actor.id || "").slice(0, 12)}`;
    result.language = String(actor.lang || "");
  } else if (type === "effect") {
    result.title = String(record(data.desc).ability || "Effect");
    result.value = data.result;
  }
  return result;
}
export function localRow(entry: Entry): JournalRow {
  const result = record(entry.reply?.result);
  return {
    id: `local-${entry.id}`,
    kind: entry.mode,
    title: entry.name || entry.mode,
    language: entry.mode.includes("rust") ? "rust" : "ts",
    source: entry.source,
    value:
      entry.mode === "eval" && "value" in result
        ? result.value
        : entry.reply?.result,
    metadata: entry.reply,
    seq: entry.reply?.seq,
    entry,
  };
}

export function definitionHash(row: JournalRow): string {
  if (row.kind === "defined") return reference(record(row.value).hash);
  if (row.kind.startsWith("define") && row.entry?.reply?.ok)
    return reference(record(record(row.entry.reply.result).def).hash);
  return "";
}

/** Collapse only identical named identities; retain every original event. */
export function groupDefinitions(rows: JournalRow[]): JournalRow[] {
  const groups = new Map<string, JournalRow[]>();
  for (const row of rows) {
    const hash = definitionHash(row);
    if (!hash) continue;
    const key = JSON.stringify({ hash, name: row.title });
    const group = groups.get(key) ?? [];
    group.push(row);
    groups.set(key, group);
  }
  return rows.flatMap(row => {
    const hash = definitionHash(row);
    if (!hash) return [row];
    const group = groups.get(JSON.stringify({ hash, name: row.title }))!;
    return group[group.length - 1] === row
      ? [{ ...row, occurrences: group.length > 1 ? group : undefined }]
      : [];
  });
}

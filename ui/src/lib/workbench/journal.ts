import { get, writable } from "svelte/store";
import type { Command } from "./commands";
import type { Json } from "./schema";

export const HISTORY_LIMIT = 100;
type HistoryStorage = Pick<Storage, "getItem" | "setItem">;
export interface JournalEntry {
  id: number;
  command: string;
  name: string;
  values: Record<string, string>;
  startedAt: string;
  state: "running" | "completed" | "failed";
  result?: Json;
  error?: string;
}
const record = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);
function validEntry(value: unknown): value is JournalEntry {
  return (
    record(value) &&
    Number.isSafeInteger(value.id) &&
    Number(value.id) > 0 &&
    typeof value.command === "string" &&
    typeof value.name === "string" &&
    typeof value.startedAt === "string" &&
    Number.isFinite(Date.parse(value.startedAt)) &&
    record(value.values) &&
    Object.values(value.values).every((item) => typeof item === "string") &&
    typeof value.state === "string" &&
    ["running", "completed", "failed"].includes(value.state) &&
    (value.error === undefined || typeof value.error === "string") &&
    (value.state !== "completed" || Object.hasOwn(value, "result")) &&
    (value.state !== "failed" || typeof value.error === "string")
  );
}
function bounded(entries: JournalEntry[]): JournalEntry[] {
  const completed = entries
    .filter((entry) => entry.state !== "running")
    .slice(-HISTORY_LIMIT);
  const retained = new Set(completed.map((entry) => entry.id));
  return entries.filter(
    (entry) => entry.state === "running" || retained.has(entry.id),
  );
}
export function preview(value: unknown, limit = 160): string {
  const text =
    typeof value === "string" ? value : (JSON.stringify(value) ?? "");
  const compact = text.replace(/\s+/g, " ");
  return compact.length > limit ? `${compact.slice(0, limit - 1)}…` : compact;
}
export function invocationSummary(entry: JournalEntry): string {
  if (entry.command === "run") {
    const target = entry.values.target ?? "";
    const label =
      record(entry.result) && typeof entry.result.entry === "string"
        ? entry.result.entry
        : /^[a-f0-9]{64}$/.test(target)
          ? target.slice(0, 8)
          : target;
    return preview(`${label}(${entry.values.args ?? "[]"})`);
  }
  return preview(
    Object.entries(entry.values)
      .map(([key, value]) => `${key}=${preview(value, 70)}`)
      .join(" · "),
  );
}
export function resultSummary(entry: JournalEntry): string {
  const result = entry.result;
  if (record(result) && entry.command === "run")
    return `→ ${preview(result.output)}`;
  if (
    record(result) &&
    typeof result.name === "string" &&
    typeof result.hash === "string"
  )
    return `${result.name} · ${result.hash.slice(0, 8)}`;
  if (entry.command === "find" && Array.isArray(result))
    return `${result.length} definitions`;
  return preview(result);
}
/** Per-endpoint history. Only command data is captured; transport credentials never enter it. */
export class Journal {
  private nextId = 0;
  private entries = writable<JournalEntry[]>([]);
  private storage?: HistoryStorage;
  private storageKey?: string;
  readonly storageError = writable<string | null>(null);
  subscribe = this.entries.subscribe;

  connect(endpoint: string, storage?: HistoryStorage): void {
    const url = new URL(endpoint);
    const key = `repl-history:v1:${url.origin}${url.pathname.replace(/\/$/, "")}`;
    if (key === this.storageKey) return;
    if (get(this.entries).some((entry) => entry.state === "running")) {
      throw new Error(
        "Wait for running commands before switching history endpoints.",
      );
    }
    this.storageKey = key;
    this.storage = undefined;
    this.entries.set([]);
    this.nextId = 0;
    this.storageError.set(null);
    try {
      this.storage = storage ?? localStorage;
      const raw = this.storage.getItem(key);
      if (raw === null) return;
      const saved: unknown = JSON.parse(raw);
      if (
        !record(saved) ||
        saved.version !== 1 ||
        !Array.isArray(saved.entries) ||
        !saved.entries.every(validEntry) ||
        !Number.isSafeInteger(saved.nextId) ||
        Number(saved.nextId) < 0 ||
        saved.entries.some(
          (entry, index, entries) =>
            entry.id > Number(saved.nextId) ||
            (index > 0 && entry.id <= entries[index - 1]!.id),
        )
      ) {
        throw new Error("Saved history has an invalid format.");
      }
      this.nextId = Number(saved.nextId);
      this.entries.set(
        bounded(
          saved.entries.map((entry: JournalEntry) =>
            entry.state === "running"
              ? {
                  ...entry,
                  state: "failed",
                  error:
                    "Interrupted before completion; the server outcome is unknown.",
                }
              : entry,
          ),
        ),
      );
      this.persist();
    } catch (error) {
      this.storage = undefined;
      this.storageError.set(`History could not be restored: ${String(error)}`);
    }
  }
  private persist(): void {
    if (!this.storage || !this.storageKey) return;
    try {
      this.storage.setItem(this.storageKey, this.exportJson());
      this.storageError.set(null);
    } catch (error) {
      this.storageError.set(
        `History is available in this tab but could not be saved: ${String(error)}`,
      );
    }
  }
  exportJson(): string {
    return JSON.stringify(
      { version: 1, nextId: this.nextId, entries: get(this.entries) },
      null,
      2,
    );
  }
  clear(): boolean {
    if (get(this.entries).some((entry) => entry.state === "running"))
      return false;
    this.entries.set([]);
    this.persist();
    return true;
  }
  begin(command: Command, values: Record<string, string>): number {
    const id = ++this.nextId;
    this.entries.update((entries) =>
      bounded([
        ...entries,
        {
          id,
          command: command.id,
          name: command.name,
          values: { ...values },
          startedAt: new Date().toISOString(),
          state: "running",
        },
      ]),
    );
    this.persist();
    return id;
  }
  finish(id: number, result: Json) {
    this.entries.update((entries) =>
      bounded(
        entries.map((entry) =>
          entry.id === id
            ? { ...entry, state: "completed", result: structuredClone(result) }
            : entry,
        ),
      ),
    );
    this.persist();
  }
  fail(id: number, error: string) {
    this.entries.update((entries) =>
      bounded(
        entries.map((entry) =>
          entry.id === id ? { ...entry, state: "failed", error } : entry,
        ),
      ),
    );
    this.persist();
  }
}

import { writable } from "svelte/store";
import type { Command } from "./commands";
import type { Json } from "./schema";

export interface JournalEntry {
  id: number;
  command: string;
  name: string;
  values: Record<string, string>;
  state: "running" | "completed" | "failed";
  result?: Json;
  error?: string;
}
/** Session command history; transport credentials and background refreshes are excluded. */
export class Journal {
  private nextId = 0;
  private entries = writable<JournalEntry[]>([]);
  subscribe = this.entries.subscribe;
  begin(command: Command, values: Record<string, string>): number {
    const id = ++this.nextId;
    this.entries.update((entries) => [
      ...entries,
      {
        id,
        command: command.id,
        name: command.name,
        values: { ...values },
        state: "running",
      },
    ]);
    return id;
  }
  finish(id: number, result: Json) {
    this.entries.update((entries) =>
      entries.map((entry) =>
        entry.id === id
          ? { ...entry, state: "completed", result: structuredClone(result) }
          : entry,
      ),
    );
  }
  fail(id: number, error: string) {
    this.entries.update((entries) =>
      entries.map((entry) =>
        entry.id === id ? { ...entry, state: "failed", error } : entry,
      ),
    );
  }
}

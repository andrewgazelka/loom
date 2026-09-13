import { get, writable } from "svelte/store";
import { commandById, parseFields, V, type Command } from "./commands";
import { definitionView, object, string, type Json, type Row } from "./schema";
import type { WorkbenchClient } from "./client";
import type { Journal } from "./journal";

type StorageAccess = Pick<Storage, "getItem" | "setItem" | "removeItem">;
export interface PanelSnapshot {
  values: Record<string, string>;
  busy: boolean;
  loadingSource: boolean;
  dirty: boolean;
  startedAt: number | null;
  elapsed: number | null;
  result?: Json;
  error: string;
  actorId: string;
}

/** Identity belongs to the command's declared fields, not unrelated explorer selections. */
export function panelKey(
  command: Command,
  defaults: Record<string, string>,
): string {
  const identity: Record<string, string> = {};
  if (command.id !== V.add) {
    for (const field of command.fields) {
      if (
        [
          "target",
          "name",
          "id",
          "hash",
          "old",
          "new",
          "def",
          "group",
          "revision",
        ].includes(field.key)
      )
        identity[field.key] = defaults[field.key] ?? "";
    }
  }
  return JSON.stringify({ command: command.id, identity });
}

/** A workspace, rather than a mounted form, owns requests and editable values. */
export class PanelSession {
  private baseline: Record<string, string>;
  private state;
  subscribe;
  set: (value: PanelSnapshot) => void;
  constructor(
    readonly command: Command,
    defaults: Record<string, string>,
    draft: Record<string, string> | undefined,
    private save: (values: Record<string, string> | undefined) => void,
  ) {
    this.baseline = Object.fromEntries(
      command.fields.map((field) => [
        field.key,
        defaults[field.key] ?? field.initial ?? "",
      ]),
    );
    this.state = writable<PanelSnapshot>({
      values: { ...(draft ?? this.baseline) },
      busy: false,
      loadingSource: false,
      dirty: draft !== undefined,
      startedAt: null,
      elapsed: null,
      error: "",
      actorId: "",
    });
    this.subscribe = this.state.subscribe;
    this.set = (value) => {
      const dirty =
        JSON.stringify(value.values) !== JSON.stringify(this.baseline);
      this.state.set({ ...value, dirty });
      this.save(dirty ? { ...value.values } : undefined);
    };
  }
  get busy() {
    return get(this.state).busy;
  }
  async prepare(client: WorkbenchClient) {
    const snapshot = get(this.state);
    if (
      this.command.id !== V.update ||
      snapshot.dirty ||
      snapshot.values.source ||
      !snapshot.values.name ||
      snapshot.busy
    )
      return;
    this.state.set({ ...snapshot, busy: true, loadingSource: true });
    try {
      const result = await client.call(commandById(V.view), {
        target: snapshot.values.name,
      });
      if (get(this.state).dirty) return;
      this.baseline = {
        ...snapshot.values,
        source: definitionView(result).source,
        expected_hash: definitionView(result).hash,
      };
      this.state.update((value) => ({
        ...value,
        values: { ...this.baseline },
        error: "",
      }));
    } catch (error) {
      this.state.update((value) => ({ ...value, error: String(error) }));
    } finally {
      this.state.update((value) => ({
        ...value,
        busy: false,
        loadingSource: false,
      }));
    }
  }
  async discard(client: WorkbenchClient) {
    if (this.busy) return;
    const values = { ...this.baseline };
    if (this.command.id === V.update) values.source = "";
    this.state.update((value) => ({
      ...value,
      values,
      dirty: false,
      result: undefined,
      error: "",
      elapsed: null,
    }));
    this.save(undefined);
    await this.prepare(client);
  }
  async execute(
    client: WorkbenchClient,
    journal: Journal,
    completed: (body: Row, result: Json) => void,
  ) {
    if (this.busy) return;
    const snapshot = get(this.state);
    const values = { ...snapshot.values };
    const entry = journal.begin(this.command, values);
    let body: Row;
    try {
      body = parseFields(this.command, values);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      journal.fail(entry, message);
      this.state.update((value) => ({
        ...value,
        result: undefined,
        error: message,
        elapsed: null,
      }));
      return;
    }
    const startedAt = Date.now();
    this.state.update((value) => ({
      ...value,
      busy: true,
      startedAt,
      elapsed: null,
      error: "",
      result: undefined,
    }));
    try {
      const result = await client.call(this.command, body);
      journal.finish(entry, result);
      this.state.update((value) => ({
        ...value,
        result,
        actorId: String(body.id ?? ""),
        elapsed: Date.now() - startedAt,
      }));
      const response =
        result !== null && typeof result === "object" && !Array.isArray(result)
          ? result
          : null;
      if (this.command.id === V.update && typeof response?.hash === "string") {
        values.expected_hash = response.hash;
        this.state.update((value) => ({ ...value, values: { ...values } }));
      }
      if (
        [V.update_repair, V.update_rebase].some(
          (verb) => verb === this.command.id,
        ) &&
        response?.update
      ) {
        const update = object(response.update, "update");
        values.revision = String(update.revision);
        this.state.update((value) => ({ ...value, values: { ...values } }));
      }
      this.baseline = values;
      this.state.update((value) => ({ ...value, dirty: false }));
      this.save(undefined);
      completed(body, result);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      journal.fail(entry, message);
      this.state.update((value) => ({
        ...value,
        error: message,
        elapsed: Date.now() - startedAt,
      }));
    } finally {
      this.state.update((value) => ({ ...value, busy: false }));
    }
  }
}

export class Workspace {
  readonly storageError = writable("");
  private sessions = new Map<string, PanelSession>();
  private drafts: Record<string, Record<string, string>> = {};
  private persisted = new Set<string>();
  private saveErrors = new Map<string, string>();
  private storage?: StorageAccess;
  private key = "";
  get busy() {
    return [...this.sessions.values()].some((session) => session.busy);
  }
  replayError(command: Command, values: Record<string, string>): string | null {
    const key = panelKey(command, values);
    const existing = this.sessions.get(key);
    if (existing?.busy)
      return "This command is still running. Wait before rerunning it.";
    if ((existing && get(existing).dirty) || this.drafts[key])
      return "This command has an unsaved draft. Run it or discard the draft before rerunning history.";
    return null;
  }
  connect(endpoint: string, storage?: StorageAccess) {
    const key = `loom.drafts.v1:${endpoint}`;
    if (key === this.key) return;
    if (this.busy)
      throw new Error(
        "Wait for running commands before changing the connection.",
      );
    this.sessions.clear();
    this.drafts = {};
    this.persisted.clear();
    this.saveErrors.clear();
    this.key = key;
    this.storage = undefined;
    this.storageError.set("");
    try {
      this.storage = storage ?? localStorage;
      const saved = this.storage.getItem(key);
      if (saved === null) return;
      let index: unknown = JSON.parse(saved);
      // Convert the earlier aggregate draft document once, verifying payloads
      // before replacing its index. A failed copy leaves the original intact.
      if (
        index !== null &&
        typeof index === "object" &&
        !Array.isArray(index)
      ) {
        const aggregate = object(index, "Saved drafts");
        const copies = Object.entries(aggregate).map(([key, value]) => {
          const fields = object(value, "Saved draft fields");
          for (const [name, value] of Object.entries(fields))
            string(value, `Saved draft ${name}`);
          return { key, serialized: JSON.stringify(fields) };
        });
        for (const copy of copies) {
          const destination = this.draftKey(copy.key);
          this.storage.setItem(destination, copy.serialized);
          if (this.storage.getItem(destination) !== copy.serialized)
            throw new Error(
              "Draft migration could not verify the copied source.",
            );
        }
        index = copies.map((copy) => copy.key);
        this.storage.setItem(key, JSON.stringify(index));
      }
      if (
        !Array.isArray(index) ||
        !index.every((item) => typeof item === "string") ||
        new Set(index).size !== index.length
      )
        throw new Error("Saved draft index must contain unique draft keys.");
      const restored: Record<string, Record<string, string>> = {};
      for (const key of index) {
        const savedDraft = this.storage.getItem(this.draftKey(key));
        if (savedDraft === null)
          throw new Error(`Saved draft ${key} is missing.`);
        const fields = object(JSON.parse(savedDraft), "Saved draft fields");
        restored[key] = Object.fromEntries(
          Object.entries(fields).map(([name, value]) => [
            name,
            string(value, `Saved draft ${name}`),
          ]),
        );
      }
      this.drafts = restored;
      this.persisted = new Set(index);
    } catch (error) {
      this.storageError.set(
        `Could not restore drafts: ${String(error)}. Saved data has not been changed.`,
      );
      this.storage = undefined;
    }
  }
  private draftKey(key: string): string {
    return `${this.key}:draft:${encodeURIComponent(key)}`;
  }
  private saveDraft(
    key: string,
    values: Record<string, string> | undefined,
  ): void {
    if (values) this.drafts[key] = values;
    else delete this.drafts[key];
    if (!this.storage) return;
    try {
      if (values) {
        this.storage.setItem(this.draftKey(key), JSON.stringify(values));
        if (!this.persisted.has(key)) {
          try {
            this.storage.setItem(
              this.key,
              JSON.stringify([...this.persisted, key]),
            );
          } catch (error) {
            // A failed index update must not leave an undiscoverable draft.
            try {
              this.storage.removeItem(this.draftKey(key));
            } catch (cleanup) {
              throw new Error(
                `${String(error)}; unindexed draft cleanup failed: ${String(cleanup)}`,
              );
            }
            throw error;
          }
          this.persisted.add(key);
        }
      } else {
        if (this.persisted.has(key)) {
          // Commit removal in the index before deleting the stored payload.
          this.storage.setItem(
            this.key,
            JSON.stringify([...this.persisted].filter((item) => item !== key)),
          );
          this.persisted.delete(key);
        }
        this.storage.removeItem(this.draftKey(key));
      }
      this.saveErrors.delete(key);
    } catch (error) {
      this.saveErrors.set(key, String(error));
    }
    this.storageError.set(
      this.saveErrors.size
        ? `Drafts remain in memory but could not be saved: ${[...this.saveErrors.values()].join("; ")}`
        : "",
    );
  }
  open(
    command: Command,
    defaults: Record<string, string>,
    replay = false,
  ): PanelSession {
    const key = panelKey(command, defaults);
    const previous = this.sessions.get(key);
    if (previous && !replay) return previous;
    // A rerun cannot replace the owner of an in-flight operation.
    if (replay) {
      const problem = this.replayError(command, defaults);
      if (problem) throw new Error(problem);
    }
    const session = new PanelSession(
      command,
      defaults,
      replay ? undefined : this.drafts[key],
      (values) => this.saveDraft(key, values),
    );
    this.sessions.set(key, session);
    // Completed results live in history; retain only a bounded set of idle panels.
    if (this.sessions.size > 30) {
      const oldest = [...this.sessions].find(
        ([candidate, value]) => candidate !== key && !value.busy,
      );
      if (oldest) this.sessions.delete(oldest[0]);
    }
    return session;
  }
}

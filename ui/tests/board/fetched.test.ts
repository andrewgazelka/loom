import { describe, expect, test } from "bun:test";
import { harness } from "./fetched.harness.svelte";

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (problem: unknown) => void;
}
function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (problem: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
/** Drain every microtask: promise callbacks and Svelte's own flush. */
const settle = () => new Promise<void>((done) => setTimeout(done, 0));

const client = { name: "client" };

describe("fetched: one request per key", () => {
  test("a re-spread props object with the same parts does not fetch again or drop the value", async () => {
    const calls: string[] = [];
    const answers = new Map<string, Deferred<string>>();
    const h = harness({ client, id: "a" }, (owner, id) => {
      if (owner === null) return null;
      calls.push(id);
      const d = deferred<string>();
      answers.set(id, d);
      return d.promise;
    });
    await settle();
    expect(calls).toEqual(["a"]);
    expect(h.value).toBeNull();
    answers.get("a")!.resolve("A");
    await settle();
    expect(h.value).toBe("A");

    h.set({ client, id: "a" });

    await settle();
    h.set({ client, id: "a" });
    await settle();
    expect(calls).toEqual(["a"]);
    expect(h.value).toBe("A");
    h.stop();
  });

  test("a changed part clears the value at once, fetches, and ignores a late answer for the old key", async () => {
    const calls: string[] = [];
    const answers = new Map<string, Deferred<string>>();
    const h = harness({ client, id: "a" }, (owner, id) => {
      if (owner === null) return null;
      calls.push(id);
      const d = deferred<string>();
      answers.set(id, d);
      return d.promise;
    });
    await settle();
    h.set({ client, id: "b" });
    await settle();
    expect(calls).toEqual(["a", "b"]);
    expect(h.value).toBeNull();
    answers.get("a")!.resolve("A late");
    await settle();
    expect(h.value).toBeNull();
    answers.get("b")!.resolve("B");
    await settle();
    expect(h.value).toBe("B");
    expect(h.error).toBeNull();

    // A new client identity is a new key too.
    h.set({ client: { name: "reconnected" }, id: "b" });
    await settle();
    expect(calls).toEqual(["a", "b", "b"]);
    expect(h.value).toBeNull();
    h.stop();
  });

  test("a rejection becomes the error for that key only; a declined run leaves both null", async () => {
    let attempt = 0;
    const h = harness({ client, id: "a" }, (owner) => {
      if (owner === null) return null;
      attempt += 1;
      return Promise.reject(new Error(`boom ${attempt}`));
    });
    await settle();
    await settle();
    expect(h.error).toBe("boom 1");
    expect(h.value).toBeNull();
    h.set({ client: null, id: "a" });
    await settle();
    expect(h.error).toBeNull();
    expect(h.value).toBeNull();
    h.stop();
  });

  test("a run that throws synchronously reports the error instead of breaking the effect", async () => {
    const h = harness({ client, id: "a" }, () => {
      throw new Error("bad query");
    });
    await settle();
    expect(h.error).toBe("bad query");
    h.stop();
  });
});

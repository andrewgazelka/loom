/**
 * One request per key.
 *
 * Pane and tab props arrive as a spread the parent rebuilds on every render, so an effect
 * that reads `client` and `id` directly re-runs whenever anything on the board moves: it
 * resets its state and fetches again (one `view` command per stream event), and the reset
 * flashes `null` through children that are mid-render on the old value. `fetched` compares
 * the key's parts and fetches only when one of them changed. The value is derived from the
 * key rather than cleared by an effect, so a key change empties it in the same batch as the
 * change, and a late answer for an old key is dropped.
 */
import { untrack } from "svelte";
import { reason } from "./detail/format";

export interface Fetched<T> {
  /** The answer for the current key; `null` while loading, after a failure, or when `run` declined. */
  readonly value: T | null;
  /** Why the current key has no answer, when the request failed. */
  readonly error: string | null;
}

/**
 * `key` reads the reactive inputs and returns their parts; `run` receives the parts and
 * returns the request, or `null` to decline (no connection, nothing to fetch). Parts compare
 * with `===`, so an object part (the client) must keep its identity between renders.
 */
export function fetched<const K extends readonly unknown[], T>(
  key: () => K,
  run: (...key: K) => Promise<T> | null,
): Fetched<T> {
  let previous: K | undefined;
  const stable = $derived.by(() => {
    const next = key();
    if (
      previous !== undefined &&
      previous.length === next.length &&
      previous.every((part, index) => part === next[index])
    )
      return previous;
    previous = next;
    return next;
  });
  interface Outcome {
    key: K;
    value: T | null;
    error: string | null;
  }
  let outcome = $state.raw<Outcome | null>(null);
  $effect(() => {
    const current = stable;
    let pending: Promise<T> | null;
    try {
      pending = untrack(() => run(...current));
    } catch (problem) {
      outcome = { key: current, value: null, error: reason(problem) };
      return;
    }
    if (pending === null) return;
    pending.then(
      (value) => {
        if (stable === current) outcome = { key: current, value, error: null };
      },
      (problem: unknown) => {
        if (stable === current)
          outcome = { key: current, value: null, error: reason(problem) };
      },
    );
  });
  const settled = $derived(
    outcome !== null && outcome.key === stable ? outcome : null,
  );
  return {
    get value() {
      return settled?.value ?? null;
    },
    get error() {
      return settled?.error ?? null;
    },
  };
}

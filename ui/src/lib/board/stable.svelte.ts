/**
 * Identity barriers for values a parent rebuilds on every render.
 *
 * Board panes and detail tabs receive their inputs as a spread the parent recomputes whenever
 * the board model moves, so any prop read inside a child depends on a derived that changes
 * identity on every event even when every value in it is the same. `memo` returns the previous
 * result while `same` holds, and Svelte's `===` check on deriveds then stops the propagation
 * there: children that read through it re-run only when a value actually changed.
 */

export interface Memo<T> {
  readonly value: T;
}

/** The previous result while `same(previous, next)` holds; `compute` runs on every dependency change. */
export function memo<T>(
  compute: () => T,
  same: (previous: T, next: T) => boolean,
): Memo<T> {
  let previous: T | undefined;
  let primed = false;
  // `previous` is only read back after a committed evaluation; an evaluation
  // Svelte discards (a read during teardown) can leave it one step ahead and
  // cost one extra recompute downstream, never a wrong value.
  const value = $derived.by(() => {
    const next = compute();
    if (primed && same(previous as T, next)) return previous as T;
    previous = next;
    primed = true;
    return next;
  });
  return {
    get value() {
      return value;
    },
  };
}

/** Same length and `===` at every position. */
export function sameParts(
  previous: readonly unknown[],
  next: readonly unknown[],
): boolean {
  return (
    previous.length === next.length &&
    previous.every((part, index) => part === next[index])
  );
}

/** Same own keys and `===` at every key. */
export function sameRecord(
  previous: Record<string, unknown>,
  next: Record<string, unknown>,
): boolean {
  const keys = Object.keys(previous);
  return (
    keys.length === Object.keys(next).length &&
    keys.every((key) => key in next && previous[key] === next[key])
  );
}

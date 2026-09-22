/**
 * Runes host for `fetched` tests: a props object the test replaces the way a parent's spread
 * does (new object, maybe the same parts), inside an effect root. Under bun the bare "svelte"
 * entry is the server build (its `flushSync` and `tick` are stubs), so the tests wait a
 * macrotask for the client runtime's own microtask flush instead of flushing by hand.
 */
import { fetched, type Fetched } from "../../src/lib/board/fetched.svelte";

export interface Props {
  client: object | null;
  id: string;
}
export interface Harness<T> {
  readonly value: T | null;
  readonly error: string | null;
  /** Replace the props object, as a parent re-render does; the flush follows on a microtask. */
  set(next: Props): void;
  stop(): void;
}

export function harness<T>(
  initial: Props,
  run: (client: object | null, id: string) => Promise<T> | null,
): Harness<T> {
  let props = $state.raw(initial);
  let result: Fetched<T> | undefined;
  const stop = $effect.root(() => {
    result = fetched(() => [props.client, props.id], run);
  });
  if (result === undefined)
    throw new Error("fetched did not run inside the effect root");
  const settled = result;
  return {
    get value() {
      return settled.value;
    },
    get error() {
      return settled.error;
    },
    set(next) {
      props = next;
    },
    stop,
  };
}

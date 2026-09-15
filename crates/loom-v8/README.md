# Loom V8 sandbox

`loom-v8` implements the shared `loom_sandbox::Sandbox` interface for JavaScript. Create a `V8Engine` with `Limits`, compile a source string, then call the sandbox with a JSON array of positional arguments and a borrowed `CallEffects` handler.

```javascript
const LOOM_SCHEMA = 'CREATE TABLE counter(value INTEGER);';

async function main(message) {
    return await loom.perform('example', {message});
}
```

`main` must be a function. Optional `LOOM_SCHEMA` must be SQL text. `loom.perform(op, args)` forwards a JSON effect descriptor to the caller, which supplies Loom's durable actor operations and capability checks. The sandbox does not grant actor access by itself. Host errors abort the invocation and retain their Rust error type, even when JavaScript catches rejected promises.

`loom.now()`, `loom.random(n)`, and `loom.sql(sql, params)` use the same effect bridge. Time and randomness therefore follow the actor's recorded effects; ambient `Date`, `Intl`, and `Math.random` are unavailable. The `loom` binding and its methods are frozen.

`loom.sql` accepts JavaScript strings, numbers, and `null` as parameters and returns row objects keyed by column name. Use `loom.sql.blob(bytes)` for a binary parameter. The helper translates values to the host's tagged SQL representation.

`loom.actors.get(cap)` wraps capability bytes in a frozen actor handle. `await loom.actors.accept(cap)` verifies and records the capability before returning its handle. `loom.actors.spawn(spec)` and `loom.actors.self()` also return handles. Handles provide `send(value)`, `sendBytes(bytes)`, `call(value, {timeoutMs})`, `reply(reference, value)`, `sendAfter(ms, value)`, and `stop(reason)`. Calls return a reference; replies arrive as later messages. The host applies the same authorization checks as for `loom.perform`.

`loom.messages.encode(value)` and `loom.messages.decode(bytes)` convert between JavaScript values and Loom message bytes. `loom.messages.json(handler)` wraps a value handler for an actor's byte-message entry point. Functions without a return value complete with `null`. Host integers outside JavaScript's exact integer range are rejected before entering the isolate.

Every invocation creates a fresh isolate. JavaScript globals and prototype changes do not persist between calls; durable state belongs in actor storage. The engine reuses compiled-code cache bytes and bounds worker concurrency and queued work. `Promise.all` can submit multiple effects, which the borrowed host handler executes serially.

A host effect can call another sandbox on the same engine, even with one worker. While waiting for host replies, workers execute queued calls in nested isolates. `max_reentrant_depth` bounds that nesting; exceeding it fails the invocation instead of waiting for a worker that cannot become free.

## Limits

`Limits` controls the execution deadline, JavaScript heap, source and JSON message sizes, pending effects, worker count, and queue capacity. Cancellation terminates guest execution and closes its host bridge. An unresolved promise with no outstanding host effect fails.

The heap limit does not bound process RSS, V8 code, or all native allocations. Use process or container resource limits when a total memory bound is required. This build enables V8 pointer compression. V8's memory-corruption sandbox cage is **not enabled** because the required official binary archive was unavailable. Isolates restrict JavaScript host APIs; they do not provide a process security boundary against a V8 memory-corruption exploit.

The bootstrap hides ArrayBuffer, typed-array, WebAssembly, and shared-memory constructors. It also removes `Temporal`, `WeakRef`, and `FinalizationRegistry` to keep ambient time and garbage-collection observations out of guest execution. The guest interface carries JSON values only. No network, filesystem, Node.js, or Deno APIs are installed.

Run the native integration tests with `cargo test -p loom-v8`. They exercise V8 execution, borrowed effects, typed host errors, cancellation, deadlines, fresh state, and message limits.

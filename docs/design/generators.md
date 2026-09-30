# Generators: entries that yield many values

Status: design, not built (2026-09-30). Asked for by Andrew for rgb's forge ("generators that yield multiple values").

## Why it is cheap here

Every host import in `crates/loom-rt/src/sharedcore/linker.rs` is an async host function (`func_wrap_async`), so a guest
already suspends inside a host call and its thread is free while it waits (`perform`, `call`, `kernel`). A generator is
that mechanism with a channel behind it: the guest calls a `loom.yield` import, the host hands the value to the consumer
and does not return to the guest until the consumer asks for the next one. No stack switching or new wasm feature is needed.

## Shape

* Guest: an entry that returns a stream, written as an ordinary function over a yielder:
  `pub fn panels(params: Params, out: &mut loom::Yield<Panel>) { for p in ... { out.emit(p); } }`.
  `emit` encodes the value (DAG-CBOR, as every result is) and calls `loom.yield(ptr, len)`; `Yield` is the
  only way to call it, so the row gains nothing new (`yield` is a fixed, pure label like `kernel`).
* Host: `Runtime::call_stream(def, entry, args) -> impl Stream<Item = Result<Vec<u8>>>`. Backpressure is a bounded
  channel (window of N values, default 4) so a slow consumer stalls the producer instead of buffering a whole mesh.
  Dropping the stream cancels the execution (the existing `cancel` path).
* Caller inside Loom: `loom::isolated::stream(hash, entry, args)` returns an iterator whose `next()` is one async
  host call. Combined with `call_many` (design: parallel calls, single-flight), a pipeline is a graph of streams.
* Outside: the `eval`/`run` API gains a streaming form (server-sent events over the existing HTTP transport).

## Identity and caching

A stream's value is the ordered list of its items, so its identity is a Merkle list: each item is a content-addressed
blob, the stream is `[item hashes]`. Consequences:

* A pure generator's finished stream goes into the result cache like any result (key: callee, entry, args, kernels).
  A consumer that reads only the first k items of a cached stream reads k blobs, not the whole thing.
* A stream that was abandoned or failed is never stored, exactly like a failed call (`isolated.rs`).
* Two consumers of the same in-flight pure generator share one execution (single-flight, same mechanism as `call_many`).
* Determinism is unchanged: item order is the guest's emit order, which is a function of its inputs.

## What to decide with rgb before building

1. Item granularity: a panel, a mesh, or a chunk of a mesh. Cost per item is one host round trip (about 0.3 to 1 us) plus
   the encode; it pays whenever an item costs more than that, which meshes always do.
2. Whether the consumer needs items in order (streams) or as they finish (a `call_many` over independent generators).
3. Whether an item may be a kernel handle (a mesh already in the blob store: 32 bytes) instead of bytes. Recommended: yes.

## Order of work

`loom.yield` import + `Yield` in the guest SDK + `Runtime::call_stream` (bounded channel, cancel on drop) with a wat-guest
test; then the Merkle-list result cache entry; then the guest-side `isolated::stream`; then the HTTP streaming form.

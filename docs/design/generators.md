# Generators: entries that yield many values

Status: host and guest half built (2026-09-30, commit 20ea501); guest-side consumer, cached finished streams and the HTTP form are not. Asked for by Andrew for rgb's forge ("generators that yield multiple values").

## Why it is cheap here

Every host import in `crates/loom-rt/src/sharedcore/linker.rs` is an async host function (`func_wrap_async`), so a guest
already suspends inside a host call and its thread is free while it waits (`perform`, `call`, `kernel`). A generator is
that mechanism with a channel behind it: the guest calls a `loom.yield` import, the host hands the value to the consumer
and does not return to the guest until the consumer asks for the next one. No stack switching or new wasm feature is needed.

## Shape (as built)

* Guest: any entry calls `loom::stream::emit(&value)` (DAG-CBOR, like every result). It returns once the consumer has
  room; `Err(StreamError::Cancelled)` means the consumer is gone and the entry should return. The entry's return value is
  the stream's result, delivered by `CallStream::finish`. A free function rather than a `Yield` parameter, so entries keep
  the one ABI (arguments decoded from the entry's own parameter types).
* Host: `Runtime::call_stream(hash, entry, args) -> CallStream` with `next()`, `next_bytes()` and `finish()`. The channel
  holds 4 values; a full channel suspends the guest inside `loom.yield_value` (its execution slot is released meanwhile).
  Dropping the `CallStream` closes the channel and aborts the task. Yield codes: 0 accepted, 1 consumer gone, 2 not started
  as a stream (a plain `call_def` of the same entry works and yields nothing), 3 `yield` not allowed by the row.
* Only the stream root yields: `EffectContext::delegated` clears the sink, so an isolated callee never yields into its caller.
* Effect label: the fixed label `yield` (driver: `stream::emit`), so a yielding callee is never result-cached and its callers must
  allow `yield`.
* Not built: `loom::isolated::stream(hash, entry, args)` (a guest consuming another guest's stream), the HTTP streaming form,
  and the cached Merkle-list result below.

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

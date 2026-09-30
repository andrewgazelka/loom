# What the game engine (rgb) needs from Loom

Source: the `game-engine` session's answer, 2026-09-30 (read = from its code, est = its estimate). Nothing
here is built yet; it orders the work in `native-backend.md` and `host-kernels.md`.

## What it told us

* **Meshes**: authoring form is `rgb_mesh::Mesh` (f64 positions, polygon faces, hard edges, skin weights, morph
  deltas, named f64 columns); runtime is plain indexed triangles with f32 attributes (`Vertex {position [f32;3],
  normal [f32;3]}`), people and hair as packed SoA buffers. Ships 100k to several M triangles (a hauler's BVH
  file is 130 MB), bodies 100k to 300k vertices (est), hair 100k+ guide strands. Changes per edit (seconds),
  never per frame. **Bulk numbers as packed byte strings is what it wants.**
* **Trust**: engine-generated content (forge jobs, ships, garments, characters) is trusted, so native is fine.
  Player-designed things are built from engine primitives; player code already runs in wasmtime with fuel.
  So: untrusted means wasm, everything else may be native.
* **Caches**: forge store keyed per job (generator + version, canonical params, input hashes; never a counter),
  BVH rkyv files per model keyed on content. Loom's store could hold all of it if values of 1 MiB and up are
  mmap-able and stamp-verified. The generator's code version must be inside the definition hash.
* **GPU**: Metal with shared-storage buffers. Best handoff is a pointer into an mmap'd store file:
  `newBufferWithBytesNoCopy` needs a page-aligned pointer and length. Return `(offset, len, stamp)` into the
  store, not bytes.
* **Latency**: per edit tens of ms is fine (60 to 70 ms warm wasm is OK); per load, cache hits must cost ms and
  batch (hundreds of garment, hair and texture jobs at once); per frame, no Loom calls.
* **Wanted**: cancellation (a slider drag aborts the stale job), streaming or partial results (hair, big
  bakes), threads and rayon inside native cells (wasmtime dropped WASI threads), a host kernel that runs a
  Metal compute and returns a shared buffer. Same-machine determinism is enough; fast-math is its default.

## What that decides

| Need | Loom today | Decision |
|---|---|---|
| Packed numbers | DAG-CBOR floats are 9 bytes each; byte strings borrow | `loom::Bytes` and typed views (`Packed<f32>`) in the SDK; engine-facing cells use them, never tuples of floats |
| Trusted native | wasm only | Script mode first (in-process cdylib, optimized by default), as in `native-backend.md` |
| Untrusted | wasm | unchanged; the wasm/native split follows the engine's own trust line |
| mmap handoff | `Store::get` returns bytes; spilled objects are files restored by clone | Add `Store::map_object(hash) -> Mapped {ptr, len, stamp}`: a read-only `mmap` of the spilled file, verified once per stamp (the existing rule), length rounded up to a page for Metal. A file mapped from offset 0 is already page-aligned, so no 16 KiB payload offset is needed for file-per-object; small inline values are copied |
| Result is a reference | results are bytes in the reply | A kernel or cell may return a `StoreRef {hash, len}` (content hash plus size, bytes stay in the store); the caller maps it. The result cache already holds hashes for spilled values |
| Cache key = generator version + params + inputs | result-cache key = (definition hash, args, kernel fingerprint) | Already the shape wanted. A definition hash covers its call graph, so the generator's code version is inside it; for native the artifact key adds (target, profile) |
| Batched, ms-cheap hits | `call_many`, `has_object`, single flight | Add a batch lookup to the result cache (`get_many`) and a batch `has_object`; hundreds of keys in one call, one SQLite transaction |
| Cancellation | epoch interruption exists inside the runtime, no API | New verb `cancel <call id>`; `eval`/`run` return or accept a call id; wasm stops at the next epoch tick; native cells poll a shared flag (`loom::cancelled()`), plus a hard stop only in worker mode |
| Streaming / partials | generators (`loom.yield_value`, `Runtime::call_stream`), no HTTP form | Build the HTTP and in-process streaming form; native cells yield through the same frame |
| Threads in cells | wasm shared-memory threads (`loom::scope`) | Native: plain `std::thread` and rayon; nothing to add in the SDK beyond letting it compile for the host |
| Metal compute kernel returning a shared buffer | host kernels take and return content-hash handles of bytes | A kernel may return a `DeviceBuffer` handle (host-owned `MTLBuffer`, shared storage) that the engine maps; it never crosses DAG-CBOR. Needs a host resource table with lifetimes |
| Fast math | wasm: `cranelift` default; native: rustc | Stable Rust has no fast-math flag; native `release` uses `-C target-cpu=native` and LLVM's default contract, and the engine's Metal kernels keep their own fast-math. Nightly intrinsics only behind an explicit profile |

## Order of work (proposed)

1. **Store `map_object` and `StoreRef`** (small, unblocks BVH and mesh handoff, benefits wasm cells too).
2. **Packed views in the SDK** (`Bytes`, `Packed<T>`), then move the rgb host-kernel adapter onto them.
3. **Cancellation** (verb plus epoch for wasm, flag for native).
4. **Native script mode** (SDK split, host-target graph, cdylib link, `target`/`profile`).
5. **Batch cache lookups**, **streaming**, **`DeviceBuffer` kernels**.

## Not asked for

No fixed arena, no per-frame calls, no cross-machine determinism.

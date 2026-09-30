# Host kernels: native BVH and physics for wasm guests

Design, 2026-09-29, with the first slice built (see **Status**). It answers the rgb session's request
(docs/spikes/rgb-forge, section 4) using rgb's real APIs, read at `/Volumes/Projects/andrewgazelka/rgb`
(paths below are in that repo).

> Sections 2 to 5 are the design as first written. What was built differs: handles are BLAKE3 content hashes, a resident BVH is a 16-entry LRU keyed by handle, ops are named `family.op` with string errors, and a kernel adds the fixed effect label `kernel` (versions in the result-cache key). Sections 7 to 8 describe the code.

## Status

Built and tested in Loom (commit `c2fd573`): `HostKernel` and `Runtime::register_kernel`
(`crates/loom-rt/src/kernel.rs`), the `loom.kernel` import (`sharedcore/linker.rs`), the guest calls
`loom::kernel::call` and `put` (`crates/loom-guest-rs/src/kernel.rs`), the `kernel` effect label in the
driver, the result-cache rules of section 5, and bulk shared-memory copies (`shared_copy.rs`).

The rgb side is `rgb-host-kernels`, a crate that implements section 3's ops over rgb's real `TriBvh`
(rgb's `geom.rs` included unmodified). It is for rgb to adopt; it lives outside both repos until they
decide where (`/Volumes/Projects/tmp/loom-rgb-spike/rgb-host-kernels`, copied to `docs/spikes/rgb-forge/`).
Differences from the first sketch below: there is no explicit `tribvh_build` op (the first query on a handle
builds and caches the BVH), the import takes a gather list of buffers so a batch and its handle are not
concatenated, the reply carries its tag byte last so the guest keeps the host's allocation with no copy, and
purity is the `kernel` row label plus the kernel-version fingerprint in the cache key, not a per-op label.

## 1. What rgb actually calls

Three different things are called "the BVH" and "Jolt" in rgb; only one is what pure generators use.

| Thing | Where | What it is | Used by generators? |
|---|---|---|---|
| `TriBvh` | `crates/rgb-mesh/src/geom.rs:285` | binary BVH over a **triangle soup** (`Vec<[DVec3; 3]>`, no indices), median split on the longest centroid axis, leaf size 4, all `f64` | **yes**: `human_groom/head.rs:147`, `character_ict/mod.rs:133`, `items/desk_computer/mod.rs:441`, the `worn/*` garments |
| `Bvh<T>` | `crates/rgb-bvh/src/lib.rs:226` | 8-wide SoA BVH over **AABB items** for interest management and LOD (`query_aabb`, `query_sphere`, `lod_cut`), `f32` relative to an origin | no: the game runtime |
| `World` | `crates/rgb-jolt/src/lib.rs:439` | Jolt physics world: bodies, shapes, constraints, `step` | no: the game runtime; `collide` (`:1069`) is its one pure-looking query |

`TriBvh`'s queries, the ones the forge uses (`geom.rs`): `nearest(p) -> (point, tri)` (`:441`),
`nearest_within(p, r)` (`:447`), `ray(origin, dir, max) -> (t, tri)` (`:491`), `overlapping(&Aabb) -> Vec<u32>`
(`:530`), `contains(p)` (`:563`). Hits in the forge: `ray` and `nearest` dominate (`gen/worn/wrinkle.rs`,
`shell.rs`, `shorts.rs`, `trim.rs`, `pressure_suit/made.rs`).

Two determinism facts from rgb that shape the design:
* `TriBvh` code is careful about FMA: `geom::mul_add` (`:39`) is an explicit multiply then add. It also relies on
  NaN being dropped by `f64::min` in `ray_box` (`geom.rs:304`): a NaN appears inside the kernel and never reaches a result.
* Jolt is built `CROSS_PLATFORM_DETERMINISTIC=ON` with `deterministicSimulation = true`
  (`rgb-jolt-sys/build.rs:61`, `rgb-jolt/src/lib.rs:588`), and rgb sorts what Jolt reports in no fixed order
  (`collide` sorts by body, `rgb-jolt/src/lib.rs:1115`).

So the first kernels to expose are the `TriBvh` queries. The AABB BVH and Jolt are out of scope for the first slice
and are described in section 6 only to fix the effect classes.

## 2. Handles are content hashes

The earlier sketch used per-execution table ids. A better shape falls out of Loom's own store: **a handle is the
BLAKE3 hash of the bytes it names.**

```
soup_put(bytes)      -> Soup   // Soup = 32-byte hash; the bytes go into the CAS once (deduplicated)
tribvh_build(soup)   -> TriBvh // TriBvh = hash of ("tribvh@1", soup hash): a name, not a pointer
```

* A handle is a **value**. It can be returned, stored, passed to another definition, and cached, and it means the
  same thing on every host. Nothing in a guest's memory is a host address.
* The host keeps derived structures (the built BVH) in a **resident cache**, keyed by (kernel, version, input
  hash), evicted by cost per byte (the GDSF policy in `loom-rt/src/result_cache.rs`, with the build time as the
  cost). A handle whose structure is not resident is rebuilt from the CAS bytes on demand; a handle whose bytes are
  not in the CAS is `NotFound`. Residency is an optimization, never state.
* `tribvh_build` is a pure function of the soup bytes, so two builds of the same soup are one build, across
  definitions and across runs.
* Forging a handle is harmless: an unknown hash is `NotFound`, a known one names bytes the caller could already
  have sent.

This removes the earlier rule "a result containing a handle is refused a cache entry": a handle is 32 bytes of
data, and results that contain one cache like any other result.

## 3. Wire format and API, `rgb-host/1`

Buffers are flat little-endian arrays, `f64` throughout (the forge's geometry is `f64`; an `f32` round trip would
change results). Every query is **batched**; there is no single-point call, so the fixed cost of a host call is
spread over `n` queries.

| op | inputs | output |
|---|---|---|
| `soup_put` | `f64[9n]` (three corners of n triangles) | `Soup` |
| `tribvh_build` | `Soup` | `TriBvh` |
| `tribvh_nearest` | `TriBvh`, `f64[3n]` points, `f64` radius (`inf` for none) | `f64[3n]` closest points, `u32[n]` triangle (`u32::MAX` for none) |
| `tribvh_ray` | `TriBvh`, `f64[7n]` (origin, direction, max) | `f64[n]` t (`0` and index `u32::MAX` for a miss), `u32[n]` triangle |
| `tribvh_overlapping` | `TriBvh`, `f64[6n]` boxes | CSR: `u32[n+1]` offsets, `u32[m]` triangle indices |
| `tribvh_contains` | `TriBvh`, `f64[3n]` points | `u8[n]` |

Sentinels, not NaN: results never contain a NaN (see the spike's NaN section).

Guest side: a `loom::kernel` module with one Rust function per op, taking slices and returning `Vec`s, so a
generator reads like the native code it replaces.

## 4. Host side: kernels are plugins, Loom stays generic

Loom must not depend on rgb. Kernels are registered by the embedder:

```rust
pub trait HostKernel: Send + Sync {
    /// "rgb-host", the family; versioned as a whole.
    fn family(&self) -> &str;
    fn version(&self) -> u32;
    /// Ops this kernel answers, each declared pure or effectful.
    fn ops(&self) -> &[OpSpec];          // OpSpec { name, pure: bool }
    fn call(&self, op: &str, args: Args<'_>, store: &Store) -> Result<Vec<u8>, KernelError>;
}
Runtime::register_kernel(Arc<dyn HostKernel>)
```

rgb (or a small `rgb-host-kernels` crate in its workspace) implements it over `rgb_mesh::geom::TriBvh` and
registers it when it embeds Loom. The core-wasm linker (`loom-rt/src/sharedcore/linker.rs`) gets one import,
`loom.kernel(op_ptr, op_len, args_ptr, args_len) -> packed`, using the same packed pointer-and-length reply as
`loom.call`, and the same `isolated_response` framing for errors.

## 5. Effect rows and the cache key

A pure op is declared in the SDK wrapper, and the driver (`tools/hash-rustc/src/effects/sdk.rs`) maps a call to the
wrapper to a row label `pure:tribvh_ray@1`.

* **Purity rule.** The result cache's `callee_is_pure` (`loom-rt/src/isolated.rs`) currently needs an empty row.
  It changes to: every label is `pure:` and the kernel family's version matches what is registered. Effectful
  labels (any label that is not `pure:`) still exclude the callee.
* **Key.** (callee hash, entry, args hash, hash of the sorted `op@version` list in the callee's row). Bumping a
  kernel's version, or fixing a bug in it, changes every dependent key without anyone remembering to invalidate.
* **Runtime check stays.** The result cache already requires the call's trace to hold no effect. A pure op never
  writes a trace entry, so a definition that only uses pure ops passes; one that also performs a real effect does
  not. (This is the same check that covers the static row undercounting.)
* **Errors are not cached**, as today.

## 6. The AABB BVH and Jolt (effect classes only)

* `rgb-bvh` `Bvh<T>` queries are pure over an immutable item list; if a generator ever needs one it is the same
  pattern: `aabbs_put`, `bvh8_build`, `bvh8_query_aabb`, all `pure:`. Not needed now.
* Jolt `World` is **stateful**. Its ops (`world_new`, `body_add`, `step`, `pose`) are effectful, world ids are
  execution-scoped, and every result is recorded in the trace so replay does not re-run Jolt. Determinism is the
  kernel's job (deterministic build and settings as above, results sorted where Jolt does not order them); Loom
  records, it does not make Jolt deterministic. The only pure Jolt op is a query over an immutable shape:
  `shape_mesh(soup) -> Shape` (a hash handle) and `shape_collide(Shape, pose, other Shape) -> touches`.
* None of the effectful ones are eligible for the result cache.

## 7. Costs, measured

`docs/spikes/rgb-forge/rgb-host-kernels/examples/bench.rs`, release build, this Mac (M5 Max, 18 cores) with
other agents holding the load average at 56 to 69. Each figure is the best of 5 to 30 runs, so it is a
best case for this loaded machine, not a worst case; no medians or spreads were recorded and none of it was taken on a
quiet machine. The "via kernel" rows call `Runtime::call_kernel` from Rust: they include the argument copy in
and the reply copy out, not the 0.3 us wasm boundary, which is the first table's second row and adds to every call.

| what | measured |
|---|---|
| wasm to host to wasm, empty op, no buffers | **307 ns** per call |
| the same with one 24-byte buffer | 363 ns per call |
| a 1 MB reply written into guest memory | 33.8 us per call (about 31 GB/s) |
| `loom.put` of a 249,200-triangle mesh (17.9 MB): hash and store | 19.4 ms, paid once per mesh |
| `TriBvh::new` on that mesh, native | 140 ms |
| first query on a new handle (fetch, parse, build) | 323 ms, paid once per mesh per resident-cache entry |
| second query, resident BVH, one `contains` point | 5.1 us |

Ray batches against that mesh, native `TriBvh::ray` on one thread versus the kernel (parallel across the batch):

| rays | native | via kernel | kernel / native |
|---|---|---|---|
| 1 | 0.54 us | 0.67 us | 1.23x |
| 100 | 104 us | 110 us | 1.05x |
| 1,000 | 2.59 ms | 0.997 ms | 0.38x |
| 10,000 | 31.1 ms | 3.10 ms | 0.10x |
| 100,000 | 649 ms | 20.3 ms | 0.03x |

The acceptance bar was "under 10% overhead at a batch of 1000 rays". It is met only because the kernel spreads a
batch over cores while the native column is one thread, as the forge calls it today. The 32x at 100,000 rays on 18
cores is more than the core count, so it is mostly a scheduler artifact of comparing one thread against a pool on a
machine at load 56 to 69; the parallel gain is real but its size is not measured here. To measure it, run the same
batch with a 1-thread pool and an N-thread pool on a quiet machine. At equal threading the overhead is the copy
plus the boundary: one ray costs about 0.67 us in the kernel plus 0.3 us at the wasm boundary, against 0.54 us
native, so single queries stay in the guest.

The first query on a mesh (323 ms) is more than the native build (140 ms): about 180 ms is fetching 17.9 MB back
from the store, verifying its hash, and parsing it into triangles. It is a one-time cost per mesh and not yet
optimized (the parse copies through an intermediate `Vec<f64>`).

## 7a. Limits of the built slice

Found by the review of the first slice; fixed and not fixed.

Fixed: a handle names only bytes `loom.put` stored (a hash of any other object looks missing); kernel calls run on a
blocking thread behind one slot per core, not on the guest pool; the import is refused in pure executions; an
oversize reply or op name is an error the guest sees, not a trap; a call during which any kernel failed is not
stored in the result cache; the rgb adapter no longer caches failed lookups, hashes its source and lockfile into its
version, checks shapes before building a mesh, bounds a call to 2^20 queries and an `overlapping` reply to 64 MB, and
rejects a NaN, negative or malformed radius.

Not fixed: a running kernel call cannot be interrupted, so the execution deadline does not stop it (the slots bound
how many run, and the adapter bounds the queries per call); blobs from `loom.put` are never collected and have no
quota; a kernel call leaves no trace entry, so replaying a recorded execution on a host with different kernels
diverges instead of failing (the kernel fingerprint should go in the trace); a cache hit answers a caller whose row
lacks `kernel` where a real run would be denied; the resident BVH cache holds 16 meshes whatever their size; the
adapter's first-query cost includes a redundant copy of the mesh bytes.

## 8. Slices

Done: (1) `HostKernel`, the import, the label and cache rules, with tests including a wat guest through the import
and a compiled Rust guest (`put` returns the BLAKE3 hash; an unknown op reaches the guest as an error message).
(2) The measurements of section 7. (3) `rgb-host-kernels` over rgb's `TriBvh`, differential test: every op
bit-equal to a direct `TriBvh` call at batch sizes 1, 2, 100, 511, 512, 513 and 5000 on a 24k-triangle mesh,
repeat calls byte-identical, bad input an error not a crash.

Not done: (4) port one `worn/*` garment step that uses `ray` and `nearest` to a guest calling the kernel, and compare its
output bytes with the forge's; a differential run on rgb's real head mesh (`human_groom/head.rs`) instead of a
synthetic sphere; a compiled Rust guest calling `rgb-host` ops (the compiled-guest check used only `put` and an
unknown op, since `loomd` has no kernels registered; an embedder must register `RgbHost` in its own binary).

## Open questions

* Whether `TriBvh` should accept triangle soups by hash from the CAS directly (zero-copy from the store's blob)
  instead of a `soup_put` copy; the store returns owned `Vec<u8>` today (`loom-store/src/objects.rs`), so the first
  version copies.
* Whether the resident cache is per runtime or per tenant. Handles are hashes, so sharing is safe; memory
  accounting per tenant is the question.
* Multi-threaded kernels: `TriBvh` queries are read-only and could run on the host thread pool in parallel per
  batch, but a parallel reduction must keep result order fixed (the same rule rgb applies to Jolt's reports).

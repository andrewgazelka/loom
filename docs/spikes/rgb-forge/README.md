# Spike: can Loom build rgb's forge assets?

Asked by the rgb session (2026-09-29, with Andrew's go-ahead). Subject: rgb's `wall_bracket`
generator (`crates/space-forge/src/gen/wall_bracket.rs` in the rgb repo), reduced to its pure
geometry: an L section extruded along x with a 2-segment round bevel (`rgb-mesh` `bevel::prism_rings`,
`build`, `ops::weld`), triangulated as fans and written as little-endian f32 positions and u32
indices. glam and eyre are replaced by plain arrays and `String` errors, with glam's operation
order (its `normalize_or_zero` multiplies by the reciprocal length), so the same text compiles as
a Loom guest and as a native crate. Fans, not rgb's ear clipping: the bytes are a stand-in for the
forge's, chosen only so they are deterministic.

Files here: `generator.rs` (native, with a `libm` feature that uses the `libm` crate as rgb does),
`guest.rs` (the Loom guest: only `sweep` and `bytes` are entries), `generator_soa.rs` (the
structure-of-arrays variant), `native-*` (the scratch cargo project) and the `.ts` drivers.
Everything ran against `loomd --release` on this Mac (M5 Max, 18 cores) while other agents held
the load average between 36 and 59, so every ratio below was measured interleaved (native, then
wasm, then native again), never against a quiet baseline.

## 1. Bytes: identical

1000 parameter sets (`params(i)`, a fixed sweep of width, depth, height, thickness and bevel; none
refused). FNV-1a 64 of each set's bytes, then of the 1000 hashes: `28c6710f51cfe390` in all of

* native, Apple's libm (`f64::sin` on macOS),
* native, the `libm` crate (what rgb uses),
* Loom, wasm, `optimize: true`, NaN canonicalization off,
* Loom, wasm, NaN canonicalization on (`LOOM_WASM_NAN_CANONICALIZATION=1`),
* the structure-of-arrays variant, native and wasm.

Set 0's full bytes (3128) were compared directly as well, not only their hash.

Limit of this evidence: this generator does not produce a NaN for these parameters (I did not check
per operation), so "canonicalization on or off is the same" says the switch is harmless here, not
that it repairs a divergence. sin, cos and atan agreed between Apple's libm, the `libm` crate and
what wasm32 links (`compiler_builtins`) on these inputs.

## 2. Speed

Per 1000 calls, best of 9, interleaved, same load (`nan.ts`, `interleave.ts`):

| | ms per 1000 calls | vs native |
|---|---|---|
| native, opt-level 2 | 35 to 44 | 1.00x |
| Loom wasm, opt-level 2, canonicalization off | 59 to 74 | 1.67x to 1.68x |
| Loom wasm, canonicalization on | 84 | 1.91x |

NaN canonicalization cost 1.14x here, not the 3.7x to 7.2x rgb measured on float-dense kernels:
this generator spends its time in allocation, a `BTreeMap` weld and index arithmetic. The wasm
figure includes instantiating the module and encoding 1000 result strings; per call that is about
60 to 74 microseconds against 35 to 44 native.

Structure of arrays (`soa.ts`): one array per coordinate and one flat corner array with an offsets
array, instead of a Vec of `[f64; 3]` and a `Vec<u32>` per face: 1.14x faster native, 1.07x wasm,
identical bytes. Worth doing for the generator's internals, not a different order of magnitude:
the time is in the weld's map and in ring construction. The place it matters more is the output
format: separate position, index and (later) normal and UV buffers are what the GPU wants and let
an unchanged attribute keep its hash.

## 3. Edit loop

Change one constant in the generator, time until the new result exists (`editloop.ts`, 8 edits,
load 48): Loom `eval` median **384 ms** (240 to 508: compile 183 to 372 ms, run 41 to 121 ms),
result hash equal to the native build's every time. The same edit in the scratch native crate:
`cargo build --release -j 2` plus one run, median **1185 ms** (1014 ms with `CARGO_INCREMENTAL=1`).
That crate has no dependencies and one file, so it flatters cargo: rgb's own numbers
(`docs/agents/workflow.md` in the rgb repo) are 2.4 s for a warm client edit, 3 to 6 minutes for a
Rust edit to space-human or space-client then render, 4 to 6.5 minutes for `forge build
human_groom`. I did not rebuild space-forge, so there is no measured Loom-versus-forge number:
only Loom against a one-file crate (3x) and against the forge's published figures.

## 4. Host API for native kernels, as handles (design only)

Nothing here is built. The rule: a mesh never crosses the wasm boundary after it exists; guests
hold handles and small buffers.

```
mesh_put(bytes)            -> mesh      // one copy in; interned by blake3 of the bytes
mesh_generate(name, args)  -> mesh      // a native generator; the result stays in host memory
bvh_build(mesh)            -> bvh       // rgb-bvh, built once per (mesh hash, options)
bvh_query_segments(bvh, segs: [f32; 6*n]) -> hits: [(u32 tri, f32 t); n]
jolt_body(mesh, settings)  -> body      // jolt_*: create / step / read back likewise
jolt_step(world, dt)       -> ()        // effectful: state changes
```

* A handle is an opaque u64 into a per-execution table; it dies with the execution. Guests cannot
  forge one (the host checks the table), and a definition whose result would contain a handle is
  refused a cache entry. What crosses is `segs` (24 bytes a segment) and `hits` (8 bytes a hit):
  a few KB per query, against about 9 MB for a 250k-triangle mesh.
* Interning is by content: `mesh_put` of the same bytes returns the same host mesh, and the BVH is
  cached by (mesh hash, build options), so building a BVH twice is one build.
* Each operation is an effect with a descriptor `{"op": "bvh_query_segments", "api": "rgb-host/1"}`.
  Effect rows are how Loom already says what a definition may do. Two classes: **pure host
  functions** (`mesh_put`, `bvh_build`, `bvh_query_segments`: deterministic, no I/O, no state a
  later call could see) appear in the row as `pure:bvh_query_segments@1`, and the result cache
  treats them as compute; **effectful** ones (`jolt_step`, anything reading a file or the clock)
  keep the callee out of the cache.
* Cache key = (callee hash, entry, args hash, hash of the sorted `op@version` list in the callee's
  row). Bumping a kernel's version, or changing what it computes, changes the key, so no stale
  answer survives a native-library update without anyone remembering to invalidate.
* Determinism is the kernel's promise, not Loom's: a `pure:` op must be bit-identical run to run
  and machine to machine (single-threaded, no fast-math). Jolt is pure only if built and configured
  for its cross-platform deterministic mode; otherwise it is effectful.
* Cost, not measured: each call is an effect round trip plus a copy of its small buffers. Loom's
  `isolated_call` round trip is recorded (`isolated_call_us` in `loom-rt`) but I did not measure a
  host effect, so the per-query overhead is unknown. The design only pays if a query batches enough
  segments per call that the round trip is small against the traversal.

## 5. Cost-aware result cache (built)

`crates/loom-rt/src/result_cache.rs`. Per stored result: compute time (`cost_ns`, measured around
the callee's run), size, hit count. Per callee: computed, compute ns, hits, ns saved, stored,
skipped, bytes; `stats` reports the ten that saved the most (`call_results.by_callee`).

* Skip if recomputing is not clearly cheaper than looking up: store only when
  `cost_ns >= 4 * (3000 ns + 0.1 ns per result byte)`.
* Evict by GreedyDual-Size-Frequency: priority `L + hits * cost / size`, lowest first, `L` rising to
  each evicted priority.

Microbenchmark (`cost_aware_eviction_saves_more_compute_than_lru_at_the_same_memory`; run with
`cargo test -p loom-rt --release result_cache -- --nocapture`): 4000 distinct calls, costs 20
microseconds to 80 ms and sizes 200 bytes to 400 KB drawn independently, Zipf popularity, 40,000
requests, 16 MB:

| policy | hit rate | compute saved |
|---|---|---|
| LRU, stores everything | 60.2% | 51.7% |
| LRU, skips cheap | 61.6% | 54.5% |
| GDSF, skips cheap | 68.8% | **83.7%** |

Synthetic: real costs and sizes are correlated in ways this does not model, and the workload is
mine, not rgb's. It shows what the policy does when cost and size vary independently by orders of
magnitude, the case where it matters.

## Recommendation

For pure generators, adopt Loom as a **second path**, not a replacement, and only after three
things hold.

Worth it now: the key is sound by construction (a definition's hash covers its items and its
dependencies, so the forge's hand-listed source dirs and their gaps go away), an edit reaches a
result in a few hundred milliseconds, and bytes matched native on 1000 parameter sets.

Must be true first:
1. **Real generators run.** This spike ported one 400-line kernel by hand. Guests are safe Rust
   with no proc macros, and non-isolated builds allow only `loom`, `serde`, `serde_json`; glam,
   `rgb-mesh` and `rgb-bvh` need either the isolated vendor path or plain-array ports. Port a
   second, larger generator (a garment panel) before deciding.
2. **The native-kernel handles exist** (section 4) and a host call is measured; without them
   BVH-heavy and Jolt-heavy jobs stay native.
3. **Determinism is decided by test, not argument:** wasm NaN payloads are unspecified, this
   generator never exercised them, and the one setting that fixes it cost 1.14x here but rgb saw
   3.7x to 7.2x on its kernels. Turn `LOOM_WASM_NAN_CANONICALIZATION` on for a float-dense kernel
   and compare against native before trusting cross-machine bytes.

Where Loom would not help: solves that mutate shared state across steps, anything needing native
code at native speed, and payloads of tens of megabytes crossing calls.

## Update: the host kernels are built

`docs/design/host-kernels.md` is now implemented for the pure triangle-soup queries, and
`rgb-host-kernels/` here is the adapter over rgb's real `TriBvh` (rgb's `geom.rs` and `check/tol.rs` are
included by path, unmodified). Results: every op bit-equal to a direct `TriBvh` call; 0.3 us per wasm-to-host
call; a batch of 1000 rays 2.6x faster than rgb's native single-thread loop, 100,000 rays 32x; a mesh crosses
the boundary once as a 32-byte handle. Numbers and limits in section 7 of the design doc.

To use it: rgb embeds `loom-rt`, registers `rgb_host_kernels::RgbHost::default()` with
`Runtime::register_kernel`, and a guest calls `loom::kernel::put(&[soup_bytes])` once, then
`loom::kernel::call("rgb-host.ray", &[&handle, &rays])`. The adapter's `Cargo.toml` uses absolute paths to
this machine's Loom and rgb checkouts; inside rgb's workspace it would depend on `rgb-mesh` and drop the
`#[path]` includes.

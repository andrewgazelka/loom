# Compilation caching at every stage

## Stages and their units

```
Rust source --(rustc frontend + LLVM backend)--> wasm module --(wasmtime + Cranelift)--> native code
```

| Stage | Unit | Reuse available | Mechanism |
|---|---|---|---|
| Rust to wasm, same definition edited | crate | per item | `-C incremental` with a directory per definition lineage; keyed by rustc's HIR fingerprints |
| Rust to wasm, same content again | crate | whole build skipped | `wasm_hash = f(entry item hash, toolchain_hash)` as a CAS memo |
| Rust to wasm, across crates | crate | none by default | codegen-backend wrapper with a content-addressed object cache (tools/cache-rustc lane) |
| wasm to native | module | per function, across modules | wasmtime `Config::enable_incremental_compilation(Arc<dyn CacheStore>)` |
| build-std for wasm32 (atomics) | toolchain | once per toolchain | shared target dir |

## wasmtime per-function cache (read: wasmtime-48.0.1/src/config.rs:418 in the cargo registry)

`enable_incremental_compilation` is behind features `incremental-cache` + `cranelift`; the
`CacheStore` trait is `wasmtime_environ::CacheStore`. Keyed by a hash of the function's
translated Cranelift IR, so identical function bodies in different modules hit. Plan: implement
`CacheStore` over the Loom CAS in loom-rt.

## The codegen backend (recalled, verified in the pinned nightly's sysroot)

rustc is a frontend (parse, expand, resolve, typeck, MIR, borrowck) plus a pluggable backend
(`rustc_codegen_ssa::traits::CodegenBackend`): LLVM by default, Cranelift and GCC exist.
`rustc_codegen_ssa::base::codegen_crate` iterates codegen units and decides reuse with
`determine_cgu_reuse` (dep-graph work products), else calls
`ExtraBackendMethods::compile_codegen_unit`. In nightly-2026-08-24 the sysroot ships
`librustc_codegen_ssa-*.rmeta` and `librustc_codegen_llvm-*.rmeta`, so a wrapper can link the
LLVM backend and intercept per-CGU compilation. Key per CGU: sorted item content hashes of its
mono items (+ substituted types), target, `rustc -vV`, codegen-affecting flags, format version.
On hit, feed the cached object through the `CguReuse::PostLto` path with a synthesized
`WorkProduct`.

## Cranelift to wasm

Cranelift has no wasm output backend; it lowers IR to native ISAs only. `rustc_codegen_cranelift`
therefore cannot produce the guest artifact. LLVM to wasm stays.

## Trigger numbers

Measure "median rustc seconds for a one-function candidate change" per lineage; the object
cache lane reports it with and without the cache. build-std is paid once per toolchain.

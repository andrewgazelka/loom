# Vendored crates

## wasmtime-internal-cranelift 48.0.1 (Apache-2.0 WITH LLVM-exception)

The registry release with one added line in `src/compiler.rs` (`function_compiler`, marked `LOOM PATCH`):
`ctx.codegen_context.func.stencil.dfg.exception_tables.clear();` after the pooled context's `clear()`.

Why: `cranelift-codegen` 0.135.3 `DataFlowGraph::clear` (`src/ir/dfg.rs`) resets `insts`, `blocks`, `signatures`,
`ext_funcs`, `constants`, `immediates`, `jump_tables`, `mem_flags` and the rest, but not `exception_tables`. Wasmtime pools
one compile context per thread, and the tables are part of the incremental-cache key (`FunctionStencil` hashes `dfg`), so
a function's key depended on what the same context had compiled before: recompiling an identical module through the same
cache kept missing (525, 208, 143, ... lookups of about 670), and a new guest cell missed about 60 functions although its
code was unchanged. With the line, identical recompiles miss 0 and warm module compile falls from about 15-25 ms to about
10 ms. A 60-function WAT module reproduces it (25 misses on an identical recompile; 0 with the fix): see
`crates/loom-rt/src/compilation_cache/tests.rs`.

Better fix upstream: clear `exception_tables` in `DataFlowGraph::clear`. Drop this patch (and the `[patch.crates-io]` entry in the
root `Cargo.toml`) once a wasmtime release carries that.

## Version guard

Cargo uses a `[patch.crates-io]` path entry only for the version it declares; after a wasmtime bump away from `=48.0.1` it
ignores the patch with a warning and builds the unpatched registry crate. `the_vendored_cranelift_backend_is_the_one_the_locked_wasmtime_uses`
(`crates/loom-build/src/artifact.rs`) fails when the workspace `Cargo.lock` holds a `wasmtime` other than the version this crate
declares, or a `wasmtime-internal-cranelift` that does not come from this directory. On a bump: vendor the matching release,
re-apply the one line above, update the `=` pin in `crates/loom-rt/Cargo.toml` and `WASMTIME_VERSION` in
`crates/loom-rt/src/host_identity.rs`, or drop the patch. Dropping the patch (the `[patch.crates-io]` entry, this
directory) means deleting or changing the guard test in the same commit: it fails on purpose when the locked
`wasmtime-internal-cranelift` no longer comes from `vendor/`. This directory is not part of the guest build fingerprint
(`build_fingerprint`): it compiles into the host runtime only.

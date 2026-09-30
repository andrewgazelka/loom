# Where a warm `eval` spends its time, and what was tried (2026-09-30)

Goal: median under 100 ms for `scripts/bench/repl-latency.sh 40 100`. Everything below was measured on this Mac (M5 Max, 18 cores)
while other agents held the load average between 30 and 120, so treat absolutes as loaded; ratios inside one interleaved A/B are
the reliable numbers. Instruments: `eval` now returns `build.stages` (the build log's `build_stages` line), `runtime_ms`, and
`stats` reports `compilation_cache`; `LOOM_DRIVER_TIMING=1` adds the driver's own timing to `build.driver_timing`.

## Breakdown of a warm cell (about 100 ms at load 30 to 40, before the changes below)

| part | ms | what it is |
|---|---|---|
| `root_rustc_ms` | 57 to 77 | the served rustc: front end about 10, incremental load about 6, effect analysis about 5, **link about 26** |
| module compile (`runtime_ms.compile_ms`) | 15 to 35 | wasmtime turning the 450 to 520 KB module into code |
| everything else in the build | about 15 | `artifact_restore` 3, `sdk_reconcile` 4, `component_encode` 5, ... |
| the guest run | 0.3 | |

## Changes that worked

* **glam out of the SDK** (`6fe79e5`): every crate the SDK links is linked into every cell; glam alone cost about 20 ms
  (`root_rustc` 77 to 58, module compile 35 to 23). It is a locked registry dependency now.
* **A pre-armed linker** (`crates/loom-link`, `tools/hash-rustc/src/serve.rs`, `crates/loom-build/src/direct/linker.rs`):
  `rust-lld` takes 22 to 25 ms just to start (it maps a 137 MB `libLLVM.dylib` before it reads its arguments; `/usr/bin/true` takes
  4.5 ms; every lld build tried, including nix's, is the same) and about 4 ms to link a guest. The compiler server starts an lld at the
  beginning of each request with a named pipe as its response file, so the startup overlaps the compile; rustc's linker is a tiny
  wrapper (`-C linker=loom-link`) that writes the real arguments into the pipe and replays lld's exit code and output. Served compile
  17 ms against 28 ms; interleaved A/B on two daemons, **82 ms against 104 ms** median, 0 failures in 40, and the compiled component
  hash is identical between the two, so the link is unchanged. Without the wrapper, the `rust-lld` path, or a server, the compile links
  as before (`LOOM_PREARM_LINKER=0` forces that).
  rustc runs its linker as `loom-link -flavor wasm ...` even with `-C linker-flavor=wasm-ld`; the wrapper drops that pair.

## Tried and rejected

* **Winch (wasmtime's baseline compiler) for cells**: refuses `wasm_threads`, which the guest's shared memory needs.
* **No incremental compilation for cells**: slower in an interleaved A/B (96 ms against 92 ms; `root_rustc` 62 against 57): it reuses
  generated code for the SDK's monomorphizations.
* **Linker options** (`--threads=1`, `--strip-all`, `--no-gc-sections`): no difference; startup is the cost, and the options were
  already minimal (`--strip-debug`, `-O0`).
* **`wasm-component-ld`** (6 ms startup): it shells out to `wasm-ld`, so no gain.
* **Raising the daemon's CPU priority**: a single thread was slowed only 1.0 to 1.2x by the load; priority barely moved the numbers.

## Open: wasmtime's function cache misses about 60 functions per new cell

Two sibling cells have byte-identical bodies for all 527 functions, yet each new cell misses about 60 lookups (of about 670), and even
recompiling one module twice through one cache keeps missing (525, 208, 143, 136, 89, ...). Keys are perfectly stable when every lookup
misses (0 of 673 differ between two compiles) and drift once hits are mixed in (49 of 673 differ when every lookup hits), serial or
parallel, so a compile leaves state that changes later functions' keys. It is upstream (Cranelift `compile_with_cache` and the pooled
per-thread context in `wasmtime-internal-cranelift` 48.0.1); the cause is not found (`Function::clear`, the constant pool and the
dedupe maps were read and reset correctly). Worth about 10 ms of the 15 to 35; the scratch programs are in
`/Volumes/Projects/tmp/loom-rt-examples/` (`cachemiss`, `keyseq*`, `clifdiff`, `modbench`).

# Largest-file scan: `all`, recursive forks, native Rust

The benchmark calls real Rust guest definitions through streamable HTTP MCP.
[`largest-all.rs`](largest-all.rs) lists directories concurrently at each tree level.
[`largest-fork.rs`](largest-fork.rs) forks one worker per child directory.
Both find the largest regular file, skip symlinks, and break size ties by relative path.

Latest measured warm medians on an Apple Silicon Mac, September 9, 2026:

| Path | Median of 7 warm scans |
| --- | ---: |
| Loom Rust `all`, including MCP and recording | **10.46 ms** |
| Loom Rust recursive `fork` / `join`, including MCP and recording | **12.00 ms** |
| Native Rust, sequential scan only | **35.20 ms** |
| Native Rust, including process launch | **37.81 ms** |

The fixture begins with 10,000 files and 256 directories. During timed rounds, one added directory contains a changing winning file, for 10,001 files and 257 directories. Every implementation must find each new winner. The scan reads metadata, not file contents. Compilation and first-call initialization are excluded; filesystem caches and compiled definitions are warm. The run reported load averages of 26.16, 40.95 and 46.06.

The native reference uses sequential traversal and individual non-following metadata reads. Loom uses parallel, batched metadata operations. These are different algorithms; the comparison does not isolate Wasm overhead or establish an advantage over equally optimized native code. The recursive example has no worker bound and is a fixture workload, not a general scanner for arbitrarily large trees.

**10/12 gates pass.** Correctness, scan latency, retained database growth and wire-size gates pass. Reply storage wait medians are 1.50 / 1.59 ms for all/fork, above the 1 ms target. Identical scans add about 22.3 / 24 KiB of database pages and transfer 167,656 / 186,932 effect bytes. Each call records one queued transaction. The standalone single-effect [`largest-walk.rs`](largest-walk.rs) measured 17.22 ms in a separate run; its target remains unmet.

The initial implementation measured 161.5 ms for `all` and 314.0 ms for fork/join. Those earlier runs had different load and storage histories; use the [staged measurements](../../docs/plan-unified-memory.md#scan-contract-change-2026-09-09) for the change history, rather than treating historical ratios as controlled experiments.

## Reproduce

Run from the repository root with Rust and Bun installed. Use a dedicated
daemon and state directory; the script creates definitions and a machine actor.

```sh
bench_dir=$(mktemp -d)
LOOM_DATA_DIR="$bench_dir/state" nix run . -- --bind 127.0.0.1:18894
```

In another terminal, set `bench_dir` to that same directory, then:

```sh
bun scripts/codex-mcp-check.ts --create-fixture "$bench_dir/tree"
rustc --edition 2024 -O scripts/bench/largest-native.rs -o "$bench_dir/native"
LOOM_URL=http://127.0.0.1:18894 LOOM_TOKEN_FILE="$bench_dir/state/token" \
  bun scripts/bench/largest.ts "$bench_dir/tree" "$bench_dir/native"
```

The runner reports build time, first-call time, every warm sample, medians,
ratios, load averages, and `N/12 scan benchmark checks pass`. Four gates check correctness. Each guest variant also has gates for median latency below 15 ms, average retained database growth below 32 KiB per identical scan, fewer than 200,000 effect bytes transferred, and median reply storage wait below 1 ms. Missing metrics fail. Transaction and checkpoint durations are reported separately to diagnose storage waits. Repeating the runner reuses builds
and filesystem caches. It removes its temporary winner directory on exit;
the fixture and isolated database remain available for inspection.

## Unified-memory integration gate

With that same dedicated daemon, fixture and native executable, run:

```sh
LOOM_URL=http://127.0.0.1:18894 LOOM_TOKEN_FILE="$bench_dir/state/token" \
  bun scripts/bench/unified-memory.ts "$bench_dir/tree" "$bench_dir/native"
```

The command reports `N/15 unified-memory gates pass` and the first failed step.
The historical command name remains stable; the shared-memory tier is canceled.
The gate executes isolated `all`/`fork` definitions, checks every changing winner,
and includes all twelve scan gates plus three compiler checks. Recording transaction counts remain diagnostic. The stats counter is sampled outside scan timing; absent counters
fail admission rather than defaulting to zero. Missing build-control execution
witnesses count as failures. It never starts a daemon or substitutes saved results.

The [implementation plan](../../docs/plan-unified-memory.md#scan-contract-change-2026-09-09) records the staged trace, codec and filesystem measurements. The earlier 7 ms estimate referred to Linux and was not a measured Mac result. `largest-walk.rs` exercises a single bounded host traversal; its performance is measured separately from the guest-driven `all` and fork/join workloads.

The five-crate delta gate uses `heck`, `strsim`, `adler2`, `version_check`, and
`cfg-if`, exercising each dependency in the resulting definition. The compiler's
unsafe-code policy applies to these crates; the benchmark does not exempt them.
For its missing-dependency control, build the `loom-build` example
`evict_artifact`, set `LOOM_ARTIFACT_EVICTOR` to that executable and
`LOOM_BENCH_DB` to the dedicated daemon's database. The control removes `heck`'s
CAS outputs and materializations, then verifies a fresh definition result and
exactly two compilation processes (dependency plus definition). These variables
must refer to the isolated benchmark state; missing settings fail that gate.

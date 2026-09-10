# Largest-file scan: `all`, recursive forks, native Rust

The benchmark calls real Rust guest definitions through streamable HTTP MCP.
`largest-all.rs` lists directories concurrently at each tree level.
`largest-fork.rs` forks one worker per child directory; each subtree advances
independently and returns its winner. Both skip symlinks and break size ties
by relative path. The native Rust baseline uses sequential traversal with the
same non-following metadata reads and tie rule; it has no Loom runtime or log.

Initial baseline on Apple Silicon macOS, September 9, 2026, with 10,000 files and
256 directories, plus one changing nested file during timed runs:

| Path | Median of 7 warm scans | Relative to native |
| --- | ---: | ---: |
| Native Rust, scan only | 25.7 ms | 1× |
| Native Rust, including process launch | 28.2 ms | 1.1× |
| Rust `all`, including MCP round trip | 161.5 ms | 6.3× |
| Rust recursive `fork`/`join`, including MCP round trip | 314.0 ms | 12.2× |

All four correctness gates passed. Recursive forks were 1.94× slower than
`all` on this mostly broad, shallow tree. Forks introduce component instances
and additional host operations; this experiment does not separately attribute
their costs. Uneven, deep trees or expensive per-subtree computation may behave
differently. The results do not establish throughput for cold disks, network
filesystems, millions of entries, or arbitrary concurrency. The recursive
example has no worker bound; it is a fixture benchmark, not a recommended
unbounded scanner for arbitrary trees.

The timings include fresh filesystem observations, guest serialization, effect
recording, and MCP transport; they are not a measure of WASM overhead alone.
Filesystem caches and compiled definitions are warm. Each round changes the
winning file size and verifies every result. File contents are not read.
The new fork definition built in 1,120 ms with existing dependency artifacts;
its first call took 389 ms. Build and first-call time are excluded from warm
medians. The `all` definition reused an existing component build.

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

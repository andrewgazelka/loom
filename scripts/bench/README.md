# Largest-file scan with scoped children

The benchmark calls [`largest-scoped.rs`](largest-scoped.rs) through streamable HTTP MCP and compares it with [`largest-native.rs`](largest-native.rs). Scoped children borrow the traversal state and perform directory listings concurrently. Both scans find the largest regular file, skip symlinks, and break size ties by relative path.

The fixture begins with 10,000 files and 256 directories. Timed rounds add a directory and change its winning file, for 10,001 files and 257 directories. Every scan must find each new winner. Compilation and first-call initialization are excluded; filesystem caches and compiled definitions are warm. Loom timings include MCP and recording, while native timing excludes process launch. The algorithms differ, so this comparison does not isolate WebAssembly overhead.

The scoped variant measured 28.63 ms on Linux on September 10, 2026, before the effect API change. The current gate has not been rerun. See the [historical measurements](../../docs/plan-unified-memory.md#scan-contract-change-2026-09-09) for removed variants.

## Reproduce

Run from the repository root with Rust and Bun installed. Use a dedicated
daemon and state directory; the script creates definitions and a machine filesystem root.

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

The runner uses `scoped`, its only supported variant, and checks seven gates.

The runner reports build time, first-call time, every warm sample, medians,
ratios, load averages, and `N/7 scan benchmark checks pass`. Three gates check correctness. Each guest variant also has gates for median latency below 15 ms, average retained database growth below 32 KiB per identical scan, fewer than 200,000 effect bytes transferred, and median reply storage wait below 1 ms. Missing metrics fail. Transaction and checkpoint durations are reported separately to diagnose storage waits. Repeating the runner reuses builds
and filesystem caches. It removes its temporary winner directory on exit;
the fixture and isolated database remain available for inspection.

## Unified-memory integration gate

With that same dedicated daemon, fixture and native executable, run:

```sh
LOOM_URL=http://127.0.0.1:18894 LOOM_TOKEN_FILE="$bench_dir/state/token" \
  bun scripts/bench/unified-memory.ts "$bench_dir/tree" "$bench_dir/native"
```

The command reports `N/10 unified-memory gates pass` and the first failed step.
The gate executes the scoped definition, checks every changing winner, and includes the seven scan gates plus three compiler checks. Recording transaction counts remain diagnostic. The stats counter is sampled outside scan timing; absent counters
fail admission rather than defaulting to zero. Missing build-control execution
witnesses count as failures. It never starts a daemon or substitutes saved results.

The [implementation plan](../../docs/plan-unified-memory.md#scan-contract-change-2026-09-09) records the staged trace, codec and filesystem measurements. The earlier 7 ms estimate referred to Linux and was not a measured Mac result. `largest-walk.rs` exercises a single bounded host traversal; its performance is measured separately from the guest-driven scoped workload.

The five-crate delta gate uses `heck`, `strsim`, `adler2`, `version_check`, and
`cfg-if`, exercising each dependency in the resulting definition. The compiler's
unsafe-code policy applies to these crates; the benchmark does not exempt them.
For its missing-dependency control, build the `loom-build` example
`evict_artifact`, set `LOOM_ARTIFACT_EVICTOR` to that executable and
`LOOM_BENCH_DB` to the dedicated daemon's database. The control removes `heck`'s
CAS outputs and materializations, then verifies a fresh definition result and
exactly two compilation processes (dependency plus definition). These variables
must refer to the isolated benchmark state; missing settings fail that gate.

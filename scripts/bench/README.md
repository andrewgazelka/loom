# Largest-file scan: `all`, recursive forks, native Rust

The benchmark calls real Rust guest definitions through streamable HTTP MCP.
`largest-all.rs` lists directories concurrently at each tree level.
`largest-fork.rs` forks one worker per child directory; each subtree advances
independently and returns its winner. Both skip symlinks and break size ties
by relative path. The native Rust baseline uses sequential traversal with the
same non-following metadata reads and tie rule; it has no Loom runtime or log.

Measured on Apple Silicon macOS, September 9, 2026, with 10,000 files and
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
ratios, and `4/4 scan benchmark checks pass`. Repeating the runner reuses builds
and filesystem caches. It removes its temporary winner directory on exit;
the fixture and isolated database remain available for inspection.

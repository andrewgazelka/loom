# Loom

**A Rust REPL without function coloring.**

Write an ordinary Rust function. List directories concurrently, call another definition, or wait for a timer. Loom suspends and resumes the guest while the host does the work. Your functions stay `fn`; effectful calls do not require `async` or `.await` throughout the call chain.

Definitions and results persist between sessions. You can inspect recorded effects, replay a call, and reuse a definition by its content hash. The same runtime serves the browser REPL and coding agents over MCP.

## Useful Rust, interactively

Inspect two directories at once:

```rust
use loom::abilities::fs;

#[loom::def]
pub fn main(machine: String) -> Vec<Vec<loom::DirEntry>> {
    loom::all([
        fs::list::desc(&machine, "src"),
        fs::list::desc(&machine, "tests"),
    ])
    .expect("directory listing failed")
}
```

`machine` identifies a host filesystem rooted at a directory you choose. Each `desc` describes an operation; `loom::all` runs both concurrently and returns their typed results in input order. A `DirEntry` has `name`, `size`, and `kind` fields. The guest has no direct filesystem access.

This scales to a useful REPL task: **find the largest file in a directory tree**. The [complete Rust example](scripts/bench/largest-all.rs) lists each level concurrently, descends into directories, and chooses the largest regular file. It skips symlinks and breaks size ties by path. A [recursive fork/join version](scripts/bench/largest-fork.rs) gives each subtree its own worker.

The short example above lists only `src` and `tests`; the recursive programs below are what we benchmark.

## 10,000 files in 10.46 ms

Warm medians from the same seven-round run on an Apple Silicon Mac, September 9, 2026:

| Largest-file scan | Median |
| --- | ---: |
| Loom Rust, concurrent `all` | **10.46 ms** |
| Loom Rust, recursive `fork` / `join` | **12.00 ms** |
| Native Rust, sequential traversal | **35.20 ms** |

Loom timings include the MCP round trip, guest execution, filesystem observations, and effect recording. The native timing excludes process launch; including launch it was 37.81 ms. Compilation and first-call initialization are excluded. Both filesystem caches and compiled definitions are warm.

The fixture starts with 10,000 files in 256 directories. Every timed round changes a nested winning file, bringing the timed tree to 10,001 files and 257 directories, and verifies the new answer. These scans read directory metadata, not file contents.

Loom uses parallel, batched filesystem operations; the native reference walks sequentially. The 3.4× difference compares those implementations. It does **not** measure Wasm overhead against equally optimized native code. The separate single-effect `fs.walk` implementation measured 17.22 ms in a different run and is still being tuned.

**10/12 scan gates pass.** Both variants meet the 15 ms latency target. The two remaining failures are reply storage waits of 1.50 ms and 1.59 ms against a target below 1 ms. The earlier 7 ms figure was an unverified Linux estimate, not a measured Mac result.

[Reproduce the benchmark](scripts/bench/README.md#reproduce) · [Benchmark source](scripts/bench/largest.ts) · [Measurements and remaining work](docs/plan-unified-memory.md#scan-contract-change-2026-09-09)

## Timers use the same interface

```rust
use loom::abilities::sleep;

#[loom::def]
pub fn main() -> String {
    loom::all([sleep::desc(100), sleep::desc(200)]).expect("sleep failed");
    "both finished".into()
}
```

The host owns the timers. The guest suspends until both finish, then continues in the same ordinary function. `all` submits effects; `fork` and `join` run and collect other definitions. Actors add persistent state and event history when a task needs to live beyond one call.

## Try it

```sh
nix run .
```

Open **http://localhost:8787** for the browser REPL. The launcher prints the token file location, and Nix supplies the guest toolchains. Rust and TypeScript definitions are currently supported.

Connect Codex to the same runtime:

```sh
bun scripts/configure-codex-mcp.ts --token-file /path/to/loom/token
```

See the [setup and API guide](docs/guide.md) and [MCP setup and verification](docs/guide.md#codex-over-mcp).

## Isolation and recorded effects

Each guest instance has a separate WebAssembly memory. Host operations cross a typed DAG-CBOR boundary, and recorded results live in a content-addressed store. HTTP and MCP envelopes use JSON; guest values and structured CAS payloads use DAG-CBOR.

Loom rejects explicit unsafe guest code as a correctness check. Safe Rust can still expose compiler or library soundness bugs, so Rust's type system is not the security boundary. Keeping guest memories separate is a deliberate architectural requirement. Neither Rust nor the complete Loom sandbox has an end-to-end formal proof. A formally verified language and toolchain are a longer-term goal; see the [isolation decision and supporting evidence](docs/plan-unified-memory.md#memory-isolation-decision).

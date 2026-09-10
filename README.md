# Loom

**A Rust REPL without function coloring.**

Write an ordinary Rust function. List directories concurrently, call another definition, or wait for a timer. Loom suspends and resumes the guest while the host does the work. Your functions stay `fn`; effectful calls do not require `async` or `.await` throughout the call chain.

Definitions and results persist between sessions. You can inspect recorded effects, replay a call, and reuse a definition by its content hash. The same runtime serves the browser REPL and coding agents over MCP.

## Build a small recursive text search

Find every file containing a string. Fork a search for each child directory, read local files while those searches run, then join the matching paths.

The [complete runnable Rust example](docs/social/fork-join-example.rs) takes `machine`, `path`, and `needle`, and returns sorted matching paths. This is its core, inside an ordinary `fn`:

```rust
let jobs = directories.into_iter().map(|path| {
    loom::fork(MAIN_DEF, MainArgs {
        machine: machine.clone(), path, needle: needle.clone(),
    }).expect("fork failed")
}).collect::<Vec<_>>();

let mut matches = Vec::new();
for file in files {
    if fs::read(&machine, &file).expect("read failed").contains(&needle) {
        matches.push(file);
    }
}
matches.extend(loom::join(jobs).expect("join failed")
    .into_iter().flatten());
```

`#[loom::def]` generates `MAIN_DEF` and the named `MainArgs` struct for the recursive call. `fs::read` returns a Rust `String`; `contains` performs a case-sensitive literal search. Collecting the jobs starts every directory search before the local file reads and the final `join`.

`machine` identifies a host filesystem rooted at a directory you choose. The example skips symlinks and fails on unreadable or invalid UTF-8 files. It reads each file into memory and returns each matching path once. It is a small teaching example, without ripgrep's regex engine, ignore-file handling, binary detection, or streaming search. Guest functions need no `async` or `.await`.

## Separate benchmark: largest-file metadata scan

The content-search example above has correctness checks, but no published timing. The following measurements are for finding the largest file by metadata, using the [fork/join scanner](scripts/bench/largest-fork.rs) and the [concurrent `all` scanner](scripts/bench/largest-all.rs).

Warm medians from the same seven-round run on an Apple Silicon Mac, September 9, 2026:

| Largest-file scan | Median |
| --- | ---: |
| Loom Rust, recursive `fork` / `join` | **12.00 ms** |
| Loom Rust, concurrent `all` | **10.46 ms** |
| Native Rust, sequential traversal | **35.20 ms** |

Loom timings include the MCP round trip, guest execution, filesystem observations, and effect recording. The native timing excludes process launch; including launch it was 37.81 ms. Compilation and first-call initialization are excluded. Both filesystem caches and compiled definitions are warm.

The fixture starts with 10,000 files in 256 directories. Every timed round changes a nested winning file, bringing the timed tree to 10,001 files and 257 directories, and verifies the new answer. These scans read directory metadata, not file contents.

Loom uses parallel, batched filesystem operations; the native reference walks sequentially. These timings compare different implementations; they do **not** isolate Wasm overhead against equally optimized native code. The separate single-effect `fs.walk` implementation measured 17.22 ms in a different run and is still being tuned.

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

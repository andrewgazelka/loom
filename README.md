# Loom

**A Rust REPL without function coloring.**

Write an ordinary Rust function. List directories concurrently, call another definition, or wait for a timer. Loom suspends and resumes the guest while the host does the work. Your functions stay `fn`; effectful calls do not require `async` or `.await` throughout the call chain.

Definitions and results persist between sessions. You can inspect recorded effects, replay a call, and reuse a definition by its content hash. The same runtime serves the browser REPL and coding agents over MCP.

## Build a small recursive text search

Find every file containing a string. Fork a search for each child directory, read local files while those searches run, then join the matching paths.

The [complete runnable Rust example](docs/social/fork-join-example.rs) takes `machine`, `path`, and `needle`, and returns sorted matching paths. This is its core, inside an ordinary `fn`:

```rust
loom::scope(|scope| {
    let jobs = directories.into_iter().map(|path| {
        scope.fork(move || search(machine, &path, needle))
            .expect("fork failed")
    }).collect::<Vec<_>>();

    let mut matches = files.into_iter().filter(|path| {
        fs::read(machine, path).expect("read failed").contains(needle)
    }).collect::<Vec<_>>();

    matches.extend(jobs.into_iter()
        .flat_map(|job| job.join().expect("join failed")));
    matches
}).expect("scope failed")
```

The helper takes `machine`, `path`, and `needle` as `&str`. Children borrow the machine and search string, and each closure owns its directory path. `job.join()` returns matching paths directly. The scope joins every child before those borrows end, including jobs whose handles were forgotten. `fs::read` returns a Rust `String`; `contains` performs a case-sensitive literal search.

`machine` identifies a host filesystem rooted at a directory you choose. The example skips symlinks and fails on unreadable or invalid UTF-8 files. It reads each file into memory and returns each matching path once. It is a small teaching example, without ripgrep's regex engine, ignore-file handling, binary detection, or streaming search. Guest functions need no `async` or `.await`.

## Separate benchmark: largest-file metadata scan

The content-search example above has correctness checks, but no published timing. The following measurements are for finding the largest file by metadata, using the [fork/join scanner](scripts/bench/largest-fork.rs) and the [concurrent `all` scanner](scripts/bench/largest-all.rs).

Historical baseline from the isolated-instance backend, before shared scoped jobs: warm medians from the same seven-round run on an Apple Silicon Mac, September 9, 2026. These are not measurements of the new shared backend:

| Largest-file scan | Median |
| --- | ---: |
| Loom Rust, recursive `fork` / `join` | **12.00 ms** |
| Loom Rust, concurrent `all` | **10.46 ms** |
| Native Rust, sequential traversal | **35.20 ms** |

Loom timings include the MCP round trip, guest execution, filesystem observations, and effect recording. The native timing excludes process launch; including launch it was 37.81 ms. Compilation and first-call initialization are excluded. Both filesystem caches and compiled definitions are warm.

The fixture starts with 10,000 files in 256 directories. Every timed round changes a nested winning file, bringing the timed tree to 10,001 files and 257 directories, and verifies the new answer. These scans read directory metadata, not file contents.

Loom uses parallel, batched filesystem operations; the native reference walks sequentially. These timings compare different implementations; they do **not** isolate Wasm overhead against equally optimized native code. The separate single-effect `fs.walk` implementation measured 17.22 ms in a different run and is still being tuned.

**That isolated-backend run passed 10/12 scan gates.** Both variants meet the 15 ms latency target. The two remaining failures are reply storage waits of 1.50 ms and 1.59 ms against a target below 1 ms. The earlier 7 ms figure was an unverified Linux estimate, not a measured Mac result.

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

The host owns the timers. The guest suspends until both finish, then continues in the same ordinary function. `all` submits effects; `scope.fork` and `job.join` run borrowed closures. Content-addressed calls to other definitions use `loom::fork` and `loom::join`. Actors add persistent state and event history when a task needs to live beyond one call.

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

Each Rust execution owns a shared WebAssembly memory; its scoped jobs borrow values in that memory. Separate executions have separate memories. Host operations cross a typed DAG-CBOR boundary, and recorded results live in a content-addressed store. HTTP and MCP envelopes use JSON; guest values and structured CAS payloads use DAG-CBOR.

Loom enforces safe-code admission for user code and untrusted dependencies, including macro bodies. It rejects untrusted build scripts and procedural macros, and the compiler rejects non-`Send` captures and escaping borrows. Pinned SDK, standard-library and compiler dependencies contain trusted unsafe internals. Stack bounds, job and memory limits, and cancellation draining are enforced separately by the runtime.

Safe Rust can still expose compiler or library soundness bugs. Sibling jobs are one trust domain; these checks are not a proven security boundary between mutually hostile jobs. Neither Rust nor the complete Loom sandbox has an end-to-end formal proof. A formally verified language and toolchain remain a longer-term goal. See the [shared execution contract and checks](docs/plan-shared-execution.md) and [supporting isolation reasoning](docs/plan-unified-memory.md#memory-isolation-decision).

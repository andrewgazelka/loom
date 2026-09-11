# Loom

**Concurrent I/O in ordinary Rust functions. No `async`, no `.await`.**

Loom is a Rust execution runtime and REPL with an algebraic-effect model implemented using WebAssembly fibers. Your code performs an effect; a fiber suspends while a host handler does the work, then resumes with the result. Effectful calls keep ordinary Rust function signatures throughout the call chain.

```rust
use loom::sleep;

#[loom::def(effects = ["sleep"])]
pub fn main() {
    loom::scope(|s| {
        let a = s.spawn(|| sleep(100)).expect("spawn");
        let b = s.spawn(|| sleep(200)).expect("spawn");
        a.join().expect("sleep");
        b.join().expect("sleep");
    });
}
```

Calling `sleep` starts a timer and suspends that child until it finishes. The two scoped children run concurrently, so their waits overlap. The host owns the timers; each WebAssembly fiber preserves its suspended Rust execution.

This applies to Loom's effect APIs. It does not make arbitrary blocking Rust libraries asynchronous.

## Try it

```sh
nix run .
```

Open **http://localhost:8787** for the browser REPL. The launcher prints the token file location, and Nix supplies the guest toolchains. Rust and TypeScript definitions are currently supported.

Connect Codex to the same runtime:

```sh
bun scripts/configure-codex-mcp.ts --token-file /path/to/loom/token
```

See the [setup and API guide](docs/guide.md) and [MCP setup](docs/guide.md#codex-over-mcp).

## Effects, handlers, and fibers

Calling an effect performs it. Use plain effect functions such as `loom::sleep(100)` or the general `loom::perform::<T>(label, args)` function. The host handles effects such as filesystem reads, timers, and model calls. WebAssembly fibers supply suspension and resumption; the handlers supply the effect's meaning.

Rust users can install and nest handlers with `loom::handle_any` or select a total set of effect names with `loom::handle`. A handler can supply a value, forward to an outer handler, or retain a one-shot continuation to resume later. The callback's own effects run in the outer context. The host is the outermost handler and implements external effects.

```rust
#[loom::def(effects = [])]
pub fn main() {
    loom::handle(["sleep"], |_, _| {
        loom::Reply::Resume(loom::Value::Null)
    }, || loom::sleep(200).expect("sleep failed"))
    .expect("handler failed");
}
```

Total handlers remove their selected effects from the body's inferred residual row. Unknown dispatch requires a declaration such as `#[loom::def(effects = ["sleep"])]`, and the runtime enforces it when an effect reaches the host. This is Loom's source checker and runtime policy, not rustc effect typing.

[Stored handler definitions](docs/content-addressed-handlers.md) can be linked by literal content hash using `loom::handle_with`. [`loom::preview::writes`](examples/rust-preview/src/lib.rs) is a guest handler that returns file diffs without applying those writes. The REPL renders those diffs. Recording and replay wrap only effects reaching the root handler; guest-handled effects are guest computation.

The compiler is unmodified Rust 1.97.0. Loom enables unstable build options through `RUSTC_BOOTSTRAP` to rebuild atomics-enabled standard libraries and configure immediate-abort panics. Guest code uses ordinary Rust syntax; this is not a stable-only compiler setup. See the [handler guide](docs/guide.md#guest-defined-effect-handlers) for continuation lifetime, inheritance, and effect-row rules.

The complete guest-handler round trip measured **13.811 µs median, 24.356 µs p99** over 10,000 warm calls on Linux on September 10, 2026, within an 8-CPU, 24-GiB allocation. The interval includes installing the handler, typed guest encoding, dispatch, resumption, and removing the handler. Before the bounded instance cache, the fixture measured 31.738 µs median. Reproduce with `bun scripts/bench/effects-handlers.ts`; the gate requires a median below 20 µs and also checks handler semantics, stored handlers, replay, and previews.

`loom::scope` provides concurrency with borrowed closures through `scope.spawn` and typed results through `job.join`. The scope waits for every child before its borrows end, including children whose handles were forgotten. `loom::spawn` accepts `'static` closures for fire-and-forget work or handles moved across tasks; detached tasks still running when the definition entry returns are cancelled without an implicit wait. `loom::call(DEF, args)` synchronously calls a separately stored definition; place the call inside a scoped child to overlap it with other work. Rust WIT component definitions and TypeScript definitions execute effect calls sequentially and have no concurrency API. Rust concurrency is available on the shared-core path used by `loom_define`.

## A useful example: recursive text search

The [complete Rust example](docs/social/spawn-join-example.rs) finds files containing a string. It spawns a search for each child directory, reads files while those searches run, then joins and sorts the matching paths. Children borrow the machine name and search string; each owns its directory path.

`fs::read` returns a `String`, so matching uses ordinary Rust `contains`. The machine argument identifies a host filesystem rooted at a directory you choose. The example skips symlinks and fails on unreadable or invalid UTF-8 files. It reads each file into memory and implements literal search, without ripgrep's regex engine, ignore-file handling, or streaming.

Definitions and results persist between REPL sessions. You can inspect recorded effects, replay a call, and reuse a definition by its content hash. The browser REPL and coding agents over MCP use the same runtime. Actors add persistent state and event history for work that outlives a call.

## Performance

The text-search example has correctness checks but no published timing. Our benchmark finds the largest file by **metadata**, without reading file contents.

September 10, 2026: seven-round warm medians on Linux with an 8-CPU, 24-GiB runtime allocation, using the shared Rust backend:

| Largest-file scan | Median |
| --- | ---: |
| Native Rust, sequential traversal | 11.21 ms |
| Loom Rust, borrowed scoped jobs | 28.63 ms |

Loom timings include the MCP round trip, guest execution, filesystem effects, and recording. Compilation and first-call initialization are excluded. Native timing excludes process launch. Loom uses parallel, batched filesystem effects; the native reference traverses sequentially. These numbers do not isolate WebAssembly overhead against equally optimized native code.

The fixture begins with 10,000 files in 256 directories. Each timed round changes a nested winning file, giving 10,001 files in 257 directories, and checks the new result. Filesystem caches and compiled definitions are warm.

The scoped variant missed the 15 ms latency target. These measurements predate the effect API change; the current seven-gate scan has not been rerun. Historical variants and their results remain in the [implementation plan](docs/plan-unified-memory.md).

[Reproduce the benchmark](scripts/bench/README.md#reproduce) using scoped jobs. See the [benchmark source](scripts/bench/largest.ts) and [shared execution checks](docs/plan-shared-execution.md).

## Isolation and recorded effects

Each Rust execution owns a shared WebAssembly memory; its scoped jobs borrow values in that memory. Separate executions have separate memories. Host effects cross a typed DAG-CBOR boundary, and recorded results live in a content-addressed store. HTTP and MCP envelopes use JSON; guest values and structured CAS payloads use DAG-CBOR.

Loom enforces safe-code admission for user code and untrusted dependencies, including macro bodies. It rejects untrusted build scripts and procedural macros, and the compiler rejects non-`Send` captures and escaping borrows. Pinned SDK, standard-library and compiler dependencies contain trusted unsafe internals. Stack bounds, job and memory limits, and cancellation draining are enforced separately by the runtime.

Safe Rust can still expose compiler or library soundness bugs. Sibling jobs are one trust domain; these checks are not a proven security boundary between mutually hostile jobs. Neither Rust nor the complete Loom sandbox has an end-to-end formal proof. A formally verified language and toolchain remain a longer-term goal. See the [shared execution contract and checks](docs/plan-shared-execution.md) and [supporting isolation reasoning](docs/plan-unified-memory.md#memory-isolation-decision).

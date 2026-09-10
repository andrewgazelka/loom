# Shared execution, 2026-09-10

The user has authorized shared memory again, with enforced safe guest code. This supersedes the earlier cancellation for sibling jobs in one execution. Separate executions remain separate trust domains with separate memories. The Rust compiler and trusted SDK/std still are not a formally proven security boundary; source safety checks do not make mutually hostile jobs safe to place in one memory.

The end-state command is:

```sh
LOOM_URL=<isolated-daemon> LOOM_TOKEN_FILE=<token-file> bun scripts/bench/shared-execution.ts
```

It compiles and calls real Rust through MCP and reports N/7. Its first positive control borrows a vector into scoped children and observes their atomic writes in the parent. Other gates cover effects, separate root-call memories, compiler rejection of unsafe/non-Send/escaping references, and recorded-effect replay. Negative controls only count after the positive control runs. This is a functional integration gate, not a formal proof or the entire concurrency stress suite.

The first real compiler/runtime baseline is **0/7**. The borrowed-capture control fails with rustc E0425 because the retained daemon's SDK has no `loom::scope`. A preceding connection refusal was a stopped-daemon setup error and is not the functional baseline.

The final native Linux run passes **7/7**, starting with an empty Cargo cache, empty guest build directory and fresh database. Supplementary `shared-actors.ts` passes **4/4**, and `shared-limits.ts` passes **6/6**. Together they cover actor history and upgrades, pure-fold effect refusal, nested jobs beyond the worker count, the 512-job lifetime limit, stack overflow, the 256 MiB memory limit, cancellation of a spinning borrowed sibling, and self-addressed definition forks. The final affected host suite passes 134 tests; core SDK checks also pass under their separate configuration. These are tested properties, not a soundness proof.

The longer-term goal remains a formally verified language and execution model. Until that exists, safe Rust checks improve admission and correctness but cannot establish isolation between mutually hostile code sharing memory. Compiler soundness defects and bugs in trusted unsafe implementations remain relevant even when user code contains no `unsafe`.

Implementation must use core Wasm: the component canonical ABI cannot carry shared memory. The runtime owns one shared memory per execution, distinct stacks/TLS per fiber, cancellation that drains children before borrowed parent storage is released, and atomic host access to guest bytes. Captures and results stay in shared guest memory; host effects still cross a validated byte boundary.

The public API is `loom::scope`, `scope.fork`, and typed `job.join`, with `Send` and lexical lifetime bounds. Shared execution must retain the existing definition/call interface and CAS artifact verification. Existing persisted artifacts need explicit ABI admission and rebuild behavior, not silent reinterpretation.

Safety enforcement must cover untrusted dependencies as well as root source. Existing root-only `-Funsafe-code` is insufficient. Trusted SDK/compiler/std internals require explicit ownership; all safety-policy changes invalidate affected cached artifacts. Macro expansion and untrusted build-time code need separate admission controls.

Admission scans untrusted package source, including inactive configurations and declarative macro bodies. It rejects explicit unsafe operations, source inclusion that escapes the scanned package, compiler-internal attributes, and untrusted procedural macros or build scripts before they execute. Compiler lint enforcement remains a second check. Trusted exceptions use repository-owned SDK paths or exact archive contents verified against pinned checksums, including the SDK's dependency graph. A package name alone cannot grant trust. This conservative policy can reject crates whose unused code contains unsafe operations.

The SDK's allocator and closure trampoline require unsafe internals. Their ownership contract is explicit: each task starts once, each stack and TLS allocation belongs to one worker, the scope retains captures until every child stops, and a trapped child aborts the execution before borrowed parent state resumes. These are implementation obligations with runtime tests, not consequences of a source lint. Host effects retain capability checks and canonical byte validation even when closure captures no longer need serialization.

Each linear-memory stack also needs lower and upper bounds. Wasm memory bounds protect the entire memory, so a large safe Rust stack frame could otherwise overwrite another allocation inside it. The compiler disables red-zone stack accesses, and the module transformation checks stack-pointer updates against bounds configured separately for each instance. A stack-overflow control must trap before neighboring memory changes. Wasmtime's native call-stack limit alone does not establish this property.

Stock WASI threads cannot simply replace the fiber scheduler: pthread joins and atomic waits can block executor workers, and current Wasmtime removed its old wasi-threads integration. Linker-generated memory initialization also contains atomic wait/notify instructions, so any opcode policy must handle verified startup rather than reject or remove initialization blindly.

The selected target is `wasm32-unknown-unknown`, with the matching pinned Rust standard library rebuilt with atomics, immediate-abort panic handling and red zones disabled. The stock WASI threads output contained waits outside initialization. The compiler lowers only the recognized serialized initializer and rejects remaining wait/notify instructions.

Guest execution uses eight shared worker threads with a global runnable queue. An actual two-child atomic rendezvous exposed starvation in Tokio's non-stealable LIFO wake-up slot after the initialization mutex was released. A yield-only control made the same module finish, and the dedicated guest executor then passed without that diagnostic yield. Root and child guest futures both use this executor; host effects remain asynchronous. Cancellation acknowledgement follows actual guest-future and Store destruction, including when every guest worker is busy.

The metadata scan remains at 10/12; the durability policy decision and TypeScript guest removal remain queued. Shared memory does not justify attributing the earlier metadata timings to the new backend or to content search.

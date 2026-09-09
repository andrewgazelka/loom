# Plan: content-addressed crates and the shared-memory fiber tier

Status: proposal, 2026-09-09. Nothing here is built. Each phase names the one
command that prints its number; a phase without a passing number is not done.

Baseline (7 warm runs, ~10,000 files, over MCP): native sequential 26 ms,
Loom `all` 162 ms, Loom recursive fork/join 314 ms.

## Phase 0: recording off the hot path (prerequisite for everything)

Where the time goes today (per effect, on the calling thread, one SQLite
connection behind one mutex, `synchronous` unset = FULL): `put_value` of the
descriptor (`loom-rt/src/lib.rs:599`), `append("effect_invoked")` (`:608`),
`effect_put` (`:722`). Three fsynced commits. Measured on this Mac: 774 such
commits = 105 ms; the same rows in one transaction = 22 ms. fork and join are
recorded the same way (`:648`, `:669`), so the recursive style pays nine
commits per directory.

Changes:
1. `fork`, `join`, `all`, `race` are scheduler operations. They do not touch
   the store. Only leaf effects (`fs.*`, `exec`, `llm`, `cas.*`, `now`,
   `random`, `sleep`, `send`) are recorded, under their existing scope path.
2. `loom-store`: one writer thread, one channel. `append`, `put_value`,
   `effect_put` push a record and return. The writer commits everything it
   drains in one transaction. `PRAGMA synchronous=NORMAL`. An in-memory map
   of pending effect results sits in front of `effect_get`.
3. Flush barrier: before any bytes leave the process (MCP reply, HTTP reply,
   WebSocket event), wait for the writer to pass the last record this request
   produced. Nothing external ever observes an unrecorded effect.
4. `all` stops cloning child descriptors (`:623`) and stops storing its own
   aggregate descriptor and result.

Control, run BEFORE writing code: set `PRAGMA synchronous=OFF` on the store
connection, re-run the scan. Expected: `all` drops to ~60 to 80 ms with no
other change. If it does not move, this diagnosis is wrong; next suspect is
the store mutex (time `self.lock()` waits).

Goal command: the existing scan benchmark. Targets: `all` <= 55 ms,
fork/join <= 70 ms. Also: commits per scan (rows added to `events` + `cas` +
`effect_results` during one scan) drops from ~780 to <= 3.

## Phase 1: crates by hash (Unison style)

Principle: a crate is a content hash; a name is a label a person attaches to
a hash; a compiled artifact is a pure function of hashes. No sccache: the
CAS is the artifact cache.

### 1a. Ingest
`loom crate add serde@1.0.210` (MCP tool `crate_add`) runs in the existing
network-only vendor phase (`loom-build/src/lib.rs`, `is_vendored`): fetch the
crates.io tarball, verify the registry checksum, unpack, store the source
tree in the CAS as a `tree` (same `Tree { entries }` as `machine.rs:290`).
Result: `{ name, version, hash, features_available }`. Names and versions are
metadata rows pointing at the hash; two people adding the same crate get the
same hash.

### 1b. Depend
A definition's manifest (`[loom.deps]` today, `loom-check/src/lib.rs:272`)
grows a `[loom.crates]` table: `serde = { hash = "<64 hex>", features =
["derive"] }`. Aliases are the Cargo names the source uses. The definition
hash covers the manifest, so a crate upgrade is a new definition hash, and
dependents keep the old hash until upgraded. `loom upgrade <old> <new>`
rewrites every definition that names `<old>` and reports the new hashes
(Unison's `upgrade`).

### 1c. Build
Stop driving Cargo for the definition crate; drive `rustc` directly. Cargo
fingerprints are path- and mtime-based and cannot accept artifacts from
elsewhere; `rustc --extern name=path.rlib` does not care where an rlib came
from. Each crate's compiled artifact key is
`blake3(source tree hash, sorted dep artifact keys, rustc -vV, target,
features, profile flags)`; the artifact is the `.rlib` + `.rmeta` stored in
the CAS under that key. Building a definition: resolve the crate graph from
the manifest (Cargo's resolver, invoked once on a generated manifest, or a
lock committed with the definition), build missing artifacts bottom-up, then
`rustc` the definition with `--extern` for each direct dep and `-L` for the
CAS-materialized rlibs. `std` for the target is rustup's prebuilt rlib (or
one CAS artifact when `-Zbuild-std` is required).

Consequences: a warm build is one rustc invocation for the definition plus
link and adapt (~0.3 s); no double `cargo` startup (`loom-rustc/build.sh:15-16`);
artifacts move between hosts through the CAS like any other blob; the
`rust-target` warm dir is retired.

### 1d. Incremental for successive versions
`-C incremental=<cache>/lineage/<definition name>` seeded from the parent
definition's incremental dir. rustc only requires the same crate name
(`loom-definition`, already constant), compiler and flags. A one-line body
change then recompiles one codegen unit. Profile: `opt-level=2, lto=false,
codegen-units=16, incremental=true, debug=false`.

Goal command: `loom define` of a 1-line change to a definition with 5 crates,
warm: wall time <= 600 ms, and `rustc` invocations per define = 1. Control:
delete the CAS artifact for one dep and confirm exactly that dep rebuilds.

## Phase 2: the shared-memory fiber tier

One shared linear memory per execution; every fiber of that execution is a
`Store` + instance over the same memory; N OS threads run M fibers;
`perform` parks a fiber, not a thread; fork and join are host operations
with rayon's laziness. No component model in this tier. WASI is not used;
one WASI-named import is implemented by us because the Rust target expects it.

### 2a. Guest target and build
Target `wasm32-wasip1-threads` (Tier 2, ships std built with atomics and
shared memory, imports its memory). Link args: `--shared-memory
--import-memory --max-memory=<per-machine cap>`. The target expects a host
import `wasi:thread-spawn` and exports `wasi_thread_start(tid, arg)`; we
implement that import as our fiber spawn. All other WASI imports
(`fd_write`, `clock_time_get`, ...) are defined as traps
(`define_unknown_imports_as_traps` is already used, `lib.rs:264`); the SDK
routes panics and output through `perform`.

If `wasip1-threads` proves unsuitable, fallback is `wasm32-unknown-unknown`
with `-C target-feature=+atomics,+bulk-memory,+mutable-globals` and
`-Zbuild-std=std,panic_abort`, plus our own `__wasm_init_tls` /
`__stack_pointer` thread entry (what wasm-bindgen-rayon does).

Verified in the installed wasmtime 48.0.1 (`~/.cargo/registry/src/*/wasmtime-48.0.1`):
`SharedMemory::new` needs `wasm_threads(true)` (`src/runtime/memory.rs:845-882`);
`memory.atomic.wait32/64` and `notify` are implemented as libcalls
(`src/runtime/vm/libcalls.rs:626-673`); the pooling allocator refuses shared
memories (`allocator/pooling/memory_pool.rs:337-343`, FIXME #4244); the
component code has no shared-memory path (14 `shared` hits, none about
memories). So: a second `Engine` with `InstanceAllocationStrategy::OnDemand`
and `wasm_threads(true)`, core modules only.

### 2b. Host runtime (`loom-rt`, new module `fibers.rs`)
- `Execution { memory: SharedMemory, workers: N, deques: [Deque<Job>; N],
  fibers: Pool<Store>, scope_root }`. One per top-level call of a threaded
  definition.
- Worker loop (one OS thread per core, or the machine's cap): reap
  completions, pop a job from the local deque and run it, else steal the
  oldest job from another worker, else park on the effect completion queue.
- `fork(fn_ptr, data_ptr) -> job id`: host call, push `(fn_ptr, data_ptr,
  scope/fork:n)` on the caller's deque, return. No instance, no copy.
- `join(job id)`: if the job is still on this worker's deque, pop it and run
  it re-entrantly on the caller's own instance (`loom_run_job(fn_ptr,
  data_ptr)` export called from inside the host function, same shadow stack,
  nested call). If stolen, suspend the caller's fiber until the thief
  reports completion. If finished, return.
- Stolen job: take a pooled `Store`, instantiate the module over the
  execution's memory (memory imported, so only vmctx/globals/tables are
  allocated), allocate a shadow stack and TLS block through the guest's own
  allocator (export `loom_alloc_stack(size)`), call `wasi_thread_start`-style
  entry with the job. On return, the Store goes back to the pool.
- `perform` from any fiber: unchanged protocol (bytes in, bytes out), but the
  occurrence key is per JOB SCOPE, not per Store: the host keeps a scope
  stack per instance, pushed on inline run and popped on return, so
  inline-vs-stolen never changes a replay key.
- Epoch deadlines TRAP in this tier (`epoch_deadline_trap`), never yield
  (`lib.rs:294` today). A fiber suspends only at `perform`. A trap in any
  fiber aborts the whole execution; the error names the scope path.
- Backpressure: if a worker's deque exceeds a threshold, `fork` runs the
  child inline before returning.
- Leaf effect concurrency cap per execution (default 64 in flight).

### 2c. Guest SDK (`loom-guest-rs`, feature `threads`)
- `#[loom::def(threads)]` emits the core-module exports (`loom_main`,
  `loom_run_job`, `loom_alloc_stack`) and the imports (`loom.perform`,
  `loom.fork`, `loom.join`, `wasi.thread-spawn`).
- `loom::scope(|s| { s.fork(|| ...); s.fork(|| ...); })` with
  `std::thread::scope` semantics: closures are `Send`, borrows are checked,
  results are moved into parent-owned slots (rayon's `StackJob` layout).
  `loom::join(a, b)` for the two-way case. No `Mutex`, no `Condvar`, no
  channels in the SDK; atomics allowed.
- Allocator: std's wasm allocator lock is believed to use
  `memory.atomic.wait32` (verify by the opcode scan in 2d on a hello-world
  build). If so, the SDK ships a `#[global_allocator]`: dlmalloc behind a
  spin lock (cmpxchg + `spin_loop`), or a size-class allocator with
  per-worker caches and remote free lists. Per-fiber arenas are NOT used:
  results allocated by a child must outlive the child.
- Values crossing `perform` stay DAG-CBOR; results may be raw bytes with a
  declared layout (`WalkTable`-style typed views), no decode step.

### 2d. Checker (`loom-check`)
- Binary rule: parse the built module and reject any of `memory.atomic.wait32`,
  `memory.atomic.wait64`, `memory.atomic.notify` (prefix 0xFE, sub-opcodes
  0x00..0x02). Error names the function index and, via the name section, the
  Rust symbol. Positive control: a module with a `Mutex` must be rejected.
- Source rule, for the earlier and friendlier message: `std::sync::Mutex`,
  `RwLock`, `Condvar`, `Barrier`, `mpsc`, `std::thread` are diagnostics in
  `#[loom::def(threads)]` sources (`rust_effects.rs` visitor).
- Effect rule: in a threaded definition every leaf effect descriptor must be
  unique within the execution; the host errors on a duplicate. `now`,
  `random`, and `llm` without an explicit `key` are rejected by the checker.

### 2e. Replay soundness
- The effect log for a threaded execution is a SET keyed by descriptor hash
  plus job scope, not a sequence.
- The host hashes the final result of every threaded execution and stores it
  with the call record. On replay, a mismatch is an error naming the
  definition and both hashes. On by default; no flag to disable.

### 2f. Tests (write-only during the lanes; one consolidated gate at the end)
- `par_sum`: `scope` over 8 chunks equals the sequential sum; run 100 times.
- `steal`: a fork whose parent sleeps in `perform(sleep)` is executed by
  another worker (assert the job ran on a different OS thread id).
- `inline`: a fork joined immediately runs on the parent's instance (assert
  no new Store was taken from the pool).
- `deadlock_is_trap`: a fiber spinning on an atomic that another parked fiber
  would set traps at the epoch deadline with a message naming its scope.
- `wait_rejected`: a module built with `std::sync::Mutex` is refused by the
  checker with the function name. Negative control: the same source without
  the Mutex passes.
- `divergence`: a definition using `fetch_add` to assign ids replays with a
  different result hash and is reported as a divergence.
- `unique_desc`: two identical `fs.list` descriptors in one execution error.

Goal command: the scan benchmark rewritten with `scope` + `fs.list`
(directory recursion via forks). Targets on this Mac: <= 12 ms; on a Linux
dev box with Phase 3: <= 8 ms. Native rayon reference on the same box printed
beside it.

## Phase 3 (Linux, optional): io_uring leaf effects
Each worker owns one ring. `fs.list` = openat2 SQE, synchronous getdents64
(no io_uring opcode exists), K `statx(STATX_SIZE, AT_SYMLINK_NOFOLLOW)` SQEs
for file entries only (d_type decides), one submit, close SQE. Results
sorted by name. Registered bounce buffers for `fs.read` (`READ_FIXED`).
`exec` stays on a spawn thread. Expected gain over the blocking pool: 1.5 to
2x on the file-system portion, warm cache.

## Order and effort (estimates)
0. Recording fix: 2 days. Unblocks every number below.
1a-1b. Crate ingest and manifest: 2 days.
1c. Direct rustc with CAS artifacts: 4 days. 1d incremental seeding: 1 day.
2a-2c. Target, host fibers, SDK: 6 days to `par_sum` green.
2d-2e. Checker rules and divergence check: 2 days.
2f. Gate: 1 day.
3. io_uring: 3 days, after measuring Phase 2 on Linux.

## Open questions to settle with one experiment each
- Does `wasm32-wasip1-threads` std's allocator lock use `memory.atomic.wait`?
  (build hello-world, run the opcode scan)
- Can an async host function call an export on the same Store re-entrantly
  in wasmtime 48 with `async_support`? (10-line test)
- Instantiate cost of a module with imported memory under OnDemand:
  measure; target <= 20 us.
- Firecracker vs cloud-hypervisor is irrelevant to this plan; the unikernel
  discussion is parked until Phase 2 numbers exist.

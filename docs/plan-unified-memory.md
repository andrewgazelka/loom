# Plan: content-addressed crates with isolated Wasm memory

Status: implementation in progress, 2026-09-09. The consolidated command is
`LOOM_URL=<isolated daemon> LOOM_TOKEN_FILE=<token file> bun scripts/bench/unified-memory.ts <fixture> <native binary>`.
It prints the passing gate count and first failure. Performance targets below
remain acceptance criteria until that command verifies them.

## Scan contract change, 2026-09-09

The earlier 7 ms figure was an estimate for Linux, not a measurement on this Mac. The current Mac acceptance target is below 15 ms for both `all` and recursive fork/join. The user's separate report measured a 5.4 ms native batched walk; that result is a filesystem-only reference, not a Loom measurement. Keep separate Wasm memories and DAG-CBOR.

The goal command is:

```sh
LOOM_URL=<isolated-daemon> LOOM_TOKEN_FILE=<token-file> bun scripts/bench/largest.ts <fixture> <native-binary>
```

It checks correctness against a native scan, including ties, symlinks, and seven changing winners. Each Loom variant must have a warm median below 15 ms, average retained database growth below 32 KiB per identical scan, fewer than 200,000 result bytes crossing the guest boundary per scan, and median reply storage wait below 1 ms. It reports load average and fails missing metrics. Database growth includes SQLite indexes and uses both allocated and used page deltas; it does not confuse checkpoint traffic with retained database growth. The consolidated command adds the three compiler gates for 15 checks total.

The first run of the extended scan command passed 4/12 checks. On the existing history database, `all` was 184.2 ms and fork/join 149.1 ms at a load average of 23.3. Identical scans added about 690 KB. Wire and storage-wait metrics were absent and failed explicitly. This loaded run is a baseline, not a controlled attribution of individual costs.

The staged measurements preserve that distinction:

| Implementation | Checks | All median | Fork/join median | Reply storage wait, all / fork |
| --- | --- | --- | --- | --- |
| Trace and typed codec, original filesystem calls | 8/12 | 17.491 ms | 19.490 ms | 1.383 / 1.638 ms |
| Pinned roots and batched filesystem calls | 10/12 | 11.896 ms | 12.382 ms | 1.778 / 1.616 ms |
| Pooled host traversal, batched metadata retained | 10/12 | 10.246 ms | 11.335 ms | 1.535 / 1.349 ms |
| Strict guest admission and replay corrections | 10/12 | 10.457 ms | 12.003 ms | 1.498 / 1.591 ms |

The second run added an average 20,480 / 26,331 bytes per identical all/fork scan and transferred 167,656 / 186,932 effect bytes. All correctness checks passed, including seven changing winners. Its load averages were 16.61, 24.92 and 24.26. The first remaining gate is reply storage wait below 1 ms. These measurements still use the existing explicit checkpoint barrier; the background durability candidate is separate and has not changed the store contract.

The native codec benchmark uses the actual 10,000-file fixture and 257 listing payloads. Encoding took 0.129 ms and decoding 0.321 ms across 200 rounds; the 167,607 encoded bytes matched the strict encoder exactly. The host `fs.walk` path has a separate target below 8 ms, which remains unmet. Its standalone full Loom median is 17.215 ms. A mixed workload measured 8.765 ms, but interleaved native scans changed macOS metadata-cache behavior; that result does not establish standalone walk latency.

A comparison of two immutable builds, with identical guest components and cloned histories, rejected separate size lookups as the production path. Batched metadata completed all/fork in 10.645 / 12.040 ms, while batched names followed by per-file size reads took 16.488 / 14.472 ms. All 48 results were correct. The implementation retains batched metadata and pooled traversal without cache-priming state.

The isolated background durability candidate passed 41 store tests and 10 native syscall/crash controls. Requests and recording made zero sync calls, while maintenance made 60 across 20 rotations. Acknowledged effects survived SIGKILL before periodic maintenance. However, adding 100 ms to each real background sync increased contended request-round medians from 33.840 to 334.886 ms through the shared connection lock. It therefore does not establish zero sync-dependent reply waits. Its WAL bound was measured for one cooperating writer and a finite workload, not arbitrary transactions or indefinitely pinned readers. Selecting a different reply policy and enforcing production capacity limits remain unresolved; main still uses its explicit checkpoint barrier.

The final correctness snapshot passed 105 affected Linux tests, including strict guest descriptor/result admission, partial actor recovery and cancellation-sensitive replay of unjoined children. An earlier broader snapshot passed 152 tests; these counts overlap and must not be added. The six native compiler checks and exact README Rust example also passed. The UI's production parser consumed 257 effects across two actual API pages; visual verification was blocked by Computer Use's native pipe startup failure.

Implementation order and contracts:

1. A completed call stores one trace object and one `call_completed` event. The trace identifies each effect by descriptor, deterministic job scope and occurrence, and references its result. Repeated descriptors and concurrent completion order must not change replay. Result blobs deduplicate. Fresh calls keep their trace in memory and do not query SQLite for newly generated scopes. Global memoization is limited to effects with a valid hermetic or explicit cache key. Pending actors retain recovery checkpoints; failure, cancellation and race outcomes must remain replayable. Historical effects stay readable through a verified migration and trace-backed projections.
2. Every store user gets the same process-crash durability contract. Use SQLite WAL with `synchronous=NORMAL`; replies wait for commit, not a disk synchronization receipt. Background checkpointing must own disk synchronization without introducing a request-path sync through WAL restart or automatic checkpointing. A queue acceptance is insufficient for an acknowledged reply. Power-loss durability is not promised. The final implementation must test native sync behavior, recovery after process termination, and bounded WAL growth.
3. Filesystem results use typed `DirEntry` values with named fields and array-shaped DAG-CBOR encoding. The host encodes typed results directly; guests decode host-produced values without canonical re-encoding. Foreign descriptors and CAS bytes still undergo strict validation where their original bytes determine an identity. Keep CID, depth, size and numeric constraints. Measure the codec on the actual scan listing shape; target below 1 ms each way.
4. Pin each machine root directory handle. Resolve filesystem operations relative to that handle, reject parent traversal, and refuse symlinks throughout resolution. macOS batches metadata through `getattrlistbulk`; Linux uses descriptor-relative directory enumeration and `statx`. Add a bounded parallel `fs.walk` effect while retaining `fs.list` for guest-driven traversal. A rename/symlink race must fail to escape the root. Store root identity so a restart cannot silently grant access to a replacement directory.

Measure recording and codec changes with the original filesystem calls first, then measure the pinned, batched filesystem implementation. The final implementation has one path; intermediate source snapshots exist only to attribute the improvement.

The earlier FULL-sync and 64 MiB SQLite page-cache experiments were reverted. The native reference program still defaults to one worker; its optional parallel mode is diagnostic and cannot substitute for improving Loom. Shared guest memory remains canceled.

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

A cold graph uses Cargo to resolve and capture the compiler contract. Cache lookup happens before a dependency compiler runs. Subsequent root edits invoke rustc directly and adapt the component in process. Artifacts produced by untrusted host build scripts or procedural macros remain graph-private; only independently verified host-code closures can publish shared artifacts.

### 1d. Incremental for successive versions

The definition root uses one stable workspace per graph and definition name, protected by the builder gate. Its compiler flags and remapped paths stay stable, so rustc can reuse the root incremental cache. Dependency compilation disables incremental output to keep portable artifacts byte-deterministic.

Verified on native Linux: 21/21 builder tests and 6/6 production checks. A warm five-crate body edit took 469 ms with exactly one rustc invocation. Evicting the selected dependency artifact required two invocations. A new graph using cached dependencies and a fresh build directory using the same CAS each required one invocation. The incremental control checked real cache reuse and executed changed code. Serde derives and the exact typed Rust README example also passed.

## Memory isolation decision

The shared-memory fiber tier is canceled at the user's request. Each execution instance keeps its own Wasm linear memory. Calls and effect results cross instance boundaries as validated DAG-CBOR values. Guest pointers, allocator state, stacks, and borrowed Rust references do not cross those boundaries.

**Rust safety checks are not a formally proven security boundary for adversarial code. This is a major architectural constraint.** Denying explicit `unsafe` is a correctness check. It cannot establish that the compiler, standard library, dependencies, or generated macro code are sound. We do not have an end-to-end formal proof of that toolchain or of the complete Loom sandbox.

Rust's [issue #25860](https://github.com/rust-lang/rust/issues/25860) documents a lifetime/variance soundness hole. The issue notes that non-higher-ranked variants were fixed while the underlying issue persists. [cve-rs](https://github.com/Speykious/cve-rs) demonstrates memory vulnerabilities using safe Rust. The [Rust Reference](https://doc.rust-lang.org/reference/behavior-considered-undefined.html) also explains how unsound internals can let safe callers trigger undefined behavior and says its undefined-behavior model is incomplete. These are reasons to retain memory isolation, not promises that a particular demonstration works on every compiler release.

Under a correct Wasm engine and host interface, corruption within a guest stays within that instance's memory. The isolation boundary depends on Wasm validation, the engine, and checked host interfaces. We must validate CBOR, sizes, and references at the host boundary rather than trust the guest compiler's safety checks. Shared-memory imports and the proposed `#[loom::def(threads)]` mode are not admitted.

The long-term goal is a formally verified guest language whose guarantees survive compilation and execution. That requires a verified compiler and runtime contract, including the memory model and trusted library operations. With those proof obligations established and checked, we could reconsider direct memory interaction, including shared memory where the proof covers it. A proof of a language fragment alone would not justify removing isolation. Separate Wasm memories and DAG-CBOR remain the current design until that stronger foundation exists.

## Remaining integration

Run the consolidated benchmark against a copied, freshly built release daemon. Its scope is the scan and crate-cache targets above. Run explicit unsafe-code rejection controls alongside it. Unit and integration tests must also cover persistence, explicit upgrades, writer failure, and compiler artifact invalidation.

After those gates pass, disable the TypeScript-to-Wasm path as separately requested. Keep persisted records readable. Linux I/O improvements must preserve isolated instance memory and the DAG-CBOR effect protocol; the canceled shared-memory performance targets are no longer acceptance criteria.

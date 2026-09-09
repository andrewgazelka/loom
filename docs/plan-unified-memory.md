# Plan: content-addressed crates with isolated Wasm memory

Status: implementation in progress, 2026-09-09. The consolidated command is
`LOOM_URL=<isolated daemon> LOOM_TOKEN_FILE=<token file> bun scripts/bench/unified-memory.ts <fixture> <native binary>`.
It prints the passing gate count and first failure. Performance targets below
remain acceptance criteria until that command verifies them.

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

Pre-code control on this checkout, using matched debug daemons and guests:
FULL `all` 384.5 ms and fork/join 485.4 ms; OFF `all` 328.3 ms and fork/join
350.0 ms. Both passed 4/4 correctness checks. Sync accounts for part of the
cost; these debug measurements do not establish the release latency target.

Goal command: the consolidated benchmark. Targets: `all` <= 55 ms,
fork/join <= 70 ms. Queued recording transactions per scan must be <= 3,
measured by the `stats.recording_commits` counter. Inserted rows are not a
transaction counter. The response barrier must checkpoint the WAL and reject
a busy or failed checkpoint before returning success.

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

## Memory isolation decision

The shared-memory fiber tier is canceled at the user's request. Each execution instance keeps its own Wasm linear memory. Calls and effect results cross instance boundaries as validated DAG-CBOR values. Guest pointers, allocator state, stacks, and borrowed Rust references do not cross those boundaries.

**Rust safety checks are not a formally proven security boundary for adversarial code. This is a major architectural constraint.** Denying explicit `unsafe` is a correctness check. It cannot establish that the compiler, standard library, dependencies, or generated macro code are sound. We do not have an end-to-end formal proof of that toolchain or of the complete Loom sandbox.

Rust's [issue #25860](https://github.com/rust-lang/rust/issues/25860) documents a lifetime/variance soundness hole. The issue notes that non-higher-ranked variants were fixed while the underlying issue persists. [cve-rs](https://github.com/Speykious/cve-rs) demonstrates memory vulnerabilities using safe Rust. The [Rust Reference](https://doc.rust-lang.org/reference/behavior-considered-undefined.html) also explains how unsound internals can let safe callers trigger undefined behavior and says its undefined-behavior model is incomplete. These are reasons to retain memory isolation, not promises that a particular demonstration works on every compiler release.

Under a correct Wasm engine and host interface, corruption within a guest stays within that instance's memory. The isolation boundary depends on Wasm validation, the engine, and checked host interfaces. We must validate CBOR, sizes, and references at the host boundary rather than trust the guest compiler's safety checks. Shared-memory imports and the proposed `#[loom::def(threads)]` mode are not admitted.

## Remaining integration

Run the consolidated benchmark against a copied, freshly built release daemon. Its scope is the recording and crate-cache targets above, plus explicit unsafe-code rejection controls. Unit and integration tests must also cover persistence, explicit upgrades, writer failure, and compiler artifact invalidation.

After those gates pass, disable the TypeScript-to-Wasm path as separately requested. Keep persisted records readable. Linux I/O improvements must preserve isolated instance memory and the DAG-CBOR effect protocol; the canceled shared-memory performance targets are no longer acceptance criteria.

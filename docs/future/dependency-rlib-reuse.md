# Definition-dependency rlib reuse across graphs

A definition that depends on another definition compiles it as a path crate
`loom-definition-<hash16>` inside the caller's Cargo workspace
(`crates/loom-build/src/materialize.rs`). The dependency set therefore shows up in
the caller's `Cargo.toml` and `Cargo.lock`, both of which feed the graph key in
`crates/loom-build/src/direct/compile.rs`, so a caller with a dependency set no
earlier caller had pays the cold path: `cargo metadata` admission, a compiler
mirror of every unit artifact, a Cargo bootstrap, and a capture of every target
file. Measured on hydra 2026-09-21: 27 s and 4 rustc invocations for one new
dependency set, while the dependency's rlib already existed in another graph.
The compiler work in that 27 s is under one second; the rest copies artifacts.

## What exists now

`crates/loom-build/src/direct/definition_rlibs.rs` records, after every cold
bootstrap, the rlib and rmeta of each `loom_definition_*` compilation unit in
the CAS index `rust_definition_rlibs`, keyed by

    blake3(definition hash, compiler identity, target, normalized Cargo.lock)

where the normalized lock has every `loom-definition-*` package and every
reference to one removed. Two callers with the same crates.io graph and
different definition dependencies produce the same key. `restore` writes a
record's files into a deps directory, verified by hash, and refuses a record
with any file missing from the CAS. Unit tests cover key stability, capture
from units, restore, and the refusals. No build path calls `lookup` or `restore`
yet.

## Why the rest is not a one-liner

An rlib links only against the exact dependency rlibs it was compiled with:
`-C metadata` hashes of serde and friends must match, and rustc checks the
stable crate ids and SVHs embedded in the rlib. Cargo derives those hashes from
the package id, features, profile, target and the metadata of each dependency
unit; path packages hash their path relative to the workspace root. Every graph
puts its root under `rust-artifacts/<key>/root-sources/<lineage>` and the SDK
crates under the repository root, so the relative paths, and with them the
metadata hashes, agree across graphs that share a lock. That is the same
property the cross-graph unit cache (`direct/artifacts.rs`) already relies on;
it is a property to be tested, not assumed.

## Remaining steps

1. **Key the graph by the crates.io set only.** In `compile.rs`, hash the
   normalized lock (`definition_rlibs::normalized_lock`) instead of the raw lock,
   and hash the manifest with every `[dependencies.<alias>]` entry whose `path`
   is under `cache/sources/` removed. Keep the definition-dependency set in a
   separate `dependency_set` digest stored beside the recipe.
   Done when: a unit test on the key function shows two manifests that differ
   only in `loom-definition-*` entries produce one key, and a change to any
   crates.io pin produces another.

2. **Synthesize `--extern` for the replay.** On the warm path, for every entry
   of `definition.deps`, look up `definition_rlibs::key(hash, context)`; when
   every dependency has a record, `restore` each into
   `target/wasm32-unknown-unknown/release/deps` and add
   `--extern <alias>=<deps>/<rlib>` (plus the `-L dependency=<deps>` the recipe
   already carries) to the root recipe before `relocate`. When any dependency
   has no record, take the cold path, which captures it. No other fallback.
   Done when: a caller with a new dependency set whose dependency was built by
   an earlier caller replays with `rustc_invocations` = 2 and its log has no
   `cargo dependency bootstrap` line; the goal command
   `scripts/bench/add-latency.sh` gains a second mode that adds a dependency,
   then a fresh caller of it, and prints `dependent-add wall_ms=<n>` under 2 s.

3. **Prove linkability across graphs.** Integration test in
   `crates/loom-build/src/direct/tests.rs` (toolchain required): build
   definition D in graph G1 (caller A depends on D), then caller B with deps
   {D, E} where E is another definition; B must replay with D's rlib restored
   from the record and rustc must accept it (no `E0460 found possibly newer
   version of crate` and no SVH mismatch).
   Done when: that test passes on Linux CI and on hydra, and a control that
   corrupts D's recorded rlib bytes in the CAS makes the replay fail closed with
   `corrupt Rust artifact in CAS`.

4. **Incremental directory per dependency lineage.** A dependency rlib rebuilt
   for a one-function change should reuse `target/incremental/<lineage>` the
   way the root does. Record the dependency's lineage (its definition name) in
   the `DefinitionRlib` record and pass `-C incremental` when its unit is
   recompiled by `graph::repair_units`.
   Done when: editing one function of D and re-adding a caller shows
   `files hard-linked` in the dependency's rustc diagnostics, as
   `root_edits_reuse_incremental_state_and_execute_new_code` asserts for the root.

5. **Delete the cold path's artifact copies.** Once step 2 lands, the cold
   bootstrap is per crates.io set only; its 27 s is dominated by
   `compiler_cache::prepare` (every unit blob copied from SQLite into the
   mirror) and `capture_artifacts` (every target file re-read and re-put).
   Both are visible now as `compiler_mirror_ms` and `artifact_capture_ms` in
   the `build_stages` log line. Copy only the units the lock reaches, and skip
   `put` for artifacts whose hash the index already holds.
   Done when: the cold bootstrap for a manifest whose crates.io set is already
   mirrored has `compiler_mirror_ms` + `artifact_capture_ms` under 2 s on hydra.

## Falsifiers

- If step 3's test shows rustc rejecting a restored rlib because Cargo's
  metadata hash differs between graphs with the same lock, the record key must
  also include the caller graph's `-C metadata` values for the dependency's
  transitive closure, and the `--extern` synthesis must copy the exact
  `extra-filename` suffixes from the record; the design then degrades to reuse
  within one lock, which the key already expresses.
- If `cargo metadata` admission (`admission_ms`) turns out to dominate the cold
  path after step 5, the admission result is a pure function of the normalized
  lock and the trusted-source policy and can be recorded beside the recipe.

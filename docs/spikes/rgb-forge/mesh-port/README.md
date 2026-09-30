# Spike: rgb-mesh's pure modules as a Loom guest (2026-09-30)

Modules: `geom`, `mesh`, `ops`, `shapes`, `hull`, `mass`, `subdiv` from `crates/rgb-mesh/src` (rgb, read only) plus
`check/tol.rs` and rgb-bvh's `Aabb` (44 lines, `rgb-bvh/src/lib.rs:42-85`). One entry, `mesh_hash(segments)`, builds meshes with
rgb's own `shapes`, `ops`, `subdiv`, `hull` and `mass` functions and returns 14 stage hashes; its effect row is empty.
Lock: glam =0.33.10, libm =0.2.16, robust =1.2.0, smallvec =1.16.2 (all crates.io, checksums equal the cached archives). (At the time glam and libm were in the SDK's lock; glam left the SDK on 2026-09-30 because linking it cost about 20 ms of every warm eval, and `libm` now needs its build script admitted, so use glam's default features.)
Full bundle, drivers and native comparison programs: `/Volumes/Projects/tmp/loom-rgb-spike/mesh-port/` (scratch; `port.patch` here is the text edit set).

## Result

It runs, and matches native bit for bit (segments 3, 7, 12, 33; stages 0 to 12 equal in every native build and in the guest), with four kinds
of text edit (`port.patch`: about 22 lines removed, 44 added; `ops.rs` and `check/tol.rs` byte-identical):

1. `eyre::ensure!` (10 sites in `shapes.rs` and `mass.rs`): crate-path macros must be in `TOOLCHAIN_MACROS`; eyre's own `build.rs` is refused too.
   Edit: a per-file `macro_rules! ensure` and a small local `eyre` shim module.
2. `rgb_bvh::Aabb` (path dependencies are refused): copied in as a module.
3. `f: &dyn Fn(..)` at two sites (`subdiv.rs`, `hull.rs`): effect analysis cannot resolve a call through `dyn Fn` (`hir.rs` `callable`, fatal by
   design). Edit: `impl Fn` generics. A policy decision, not made: treat an unresolvable `dyn Fn` as an unknown effect edge.
4. `#[derive(Default)]` with `#[default]` and `Self::Unit`: a driver ICE (`Instance::try_resolve` on `Ctor(Variant, Const)`). Fixed in Loom
   (commit a2bf9b9), so this edit is no longer needed.

Macro and attribute rules were not a problem: built-in derives, `matches!`, `assert*`, `#[cfg(test)]`, `#[test]`, `#[must_use]` and doc comments
all pass in a bundle's own files (the 7 modules use no serde, `#[inline]` or `cfg(feature)`).

## Determinism finding for rgb

Stage 13 (4000 `DQuat::from_axis_angle`, glam's own `sin_cos`) differs between native glam with default features (macOS libm) and the guest, and equals
native built with `glam/libm`. rgb's workspace has `glam = "0.33"` with default features, so its rotations are not bit-identical across platforms
today; `features = ["libm"]` fixes that (the Loom SDK's glam already uses it).

## Timing (load 43 to 45; one run at 23)

Cold, new source hash, dependencies cached: 561 to 735 ms. Warm, same source: median 131 ms (107 to 194). Warm, new argument: median 113 ms.
The warm cost is admission and hashing of the 142 KB source (about 92 ms compile, 1 ms run), not the mesh work; native is about 1 ms per process.

## Smallest change for zero edits to the moved files

Loom: treat `dyn Fn` as an unknown edge (policy). rgb: a pure `rgb-mesh-core` without eyre, with `Aabb` inside it (an `rgb-mesh-core` crate that
Loom can take as a locked registry dependency still needs a registry: publishing it would make rgb public, so a stored, hash-pinned crate tree
(`[loom.crates]`) is the route, and no command mints those pins today).

## Found on the way (fixed)

`add` of the 142 KB definition failed with `malformed JSON: SQL error` and left a `defs` row: its item document passed 1 MiB and was spilled to a
file by the new blob store, but item documents are read inside SQL. Only opaque kinds spill now (commit c9cc7e1).

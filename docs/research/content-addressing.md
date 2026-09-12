# Content addressing, Unison-style, for Rust definitions

## What Unison hashes (read: unison-lang.org/docs/language-reference/hashes)

SHA3-512 of one term or type after name resolution: every reference to another definition is
replaced by that definition's hash, bound variables are positional (de Bruijn), mutually
recursive definitions hash together as one cycle. The unit is the definition, never the file.
Renames, reordering and formatting are free. What it does not give: early cutoff (a changed
body moves every transitive caller's hash) and it is not an evaluator or a build cache.

## Rust: which layer to hash

| Layer | What names are | What is lost | Verdict |
|---|---|---|---|
| AST (`syn`) | strings; macros unexpanded; `x.foo()` unresolved | nothing, but identity is textual | rename moves the hash; method calls cannot be attributed |
| HIR (+ `typeck` results) | `DefId` per reference; method calls resolved by type; macros expanded; control flow as written | nothing semantic | the identity layer |
| MIR (+ optimisation passes) | basic blocks, temporaries, drops | branches removed, callees inlined; shape changes per toolchain | a build-cache layer, not identity |

rustc's own incremental system fingerprints HIR per item (spans stripped, `DefId`s mapped to
stable `DefPathHash`) and uses that to decide reuse of MIR and codegen queries, so HIR is the
layer the compiler itself trusts for "did this item change". (Recalled; consistent with the
driver's behaviour observed in tools/hash-rustc tests.)

## The rules implemented in tools/hash-rustc (see docs/content-addressed-code.md)

Per item after resolution and expansion: spans removed; local binders by binding position;
same-crate references replaced by the referent's content hash (topological order; a strongly
connected component hashes as one unit); cross-crate references as
`(crate name, crate hash, DefPathHash)`; generic items hashed once; trait method calls
attributed to the trait item. Entry item hash = behavior hash. Every hash has a stored
preimage (the canonical bytes) so it can be recomputed by anyone.

Over-approximation that remains: none for resolution (rustc resolves), but the impl chosen
at monomorphisation is not part of a caller's identity (the trait method is).

## The artifact chain

```
source  ->  normalized HIR per item  ->  entry hash  ->  wasm  ->  machine code  ->  actor state
defs.source_hash   item hash (+preimage)   behavior_hash   wasm_hash=f(entry,toolchain)  wasmtime cache (wasm_hash, wasmtime)   snapshots/segments
```

Each arrow is a memo table, each node a blob addressable by hash. HIR is the identity node; MIR
may be stored under `(item hash, toolchain_hash)` for tooling but is never a key.

## Why not modify rustc

The driver API (`rustc_driver`, `rustc_interface`, `rustc_private`) is how clippy and miri
extend rustc; a fork is unnecessary. Cost: pinned nightly and API churn per toolchain bump,
acceptable because `toolchain_hash` is already part of identity.

## Function-level compilation, honestly

rustc's unit is the crate. Within a lineage, `-C incremental` reuses per item. Across crates,
identical items hash the same for identity but compile twice unless a codegen-backend wrapper
caches objects by content (see compilation-caching.md). Shared code should live in
dependency crates, which are compiled once and cached by hash; two definitions pasting the
same function is visible as one item hash in two lineages.

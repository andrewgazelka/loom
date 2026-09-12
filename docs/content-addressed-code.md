# Content-addressed Rust definitions

`tools/hash-rustc` assigns BLAKE3 identities to Rust definitions inside rustc. It runs the normal compiler pipeline and reads HIR after analysis, with name resolution, macro expansion, and type checking complete. It does not hash MIR, optimized instructions, source text, or a `syn` approximation of Rust resolution.

The driver is an independent Cargo workspace. The root workspace excludes `tools/*`, so ordinary Loom crates do not acquire a nightly dependency.

## Build and invoke

The exact pinned toolchain is `nightly-2026-08-24-aarch64-apple-darwin`. The portable toolchain file selects `nightly-2026-08-24`, with rustc-dev, llvm-tools, rust-src, clippy, and rustfmt. The verified compiler is `rustc 1.100.0-nightly (fb6531d55 2026-08-23)`, commit `fb6531d550e0075b9eb9a51464f404805eec87d9`, LLVM 23.1.0.

```sh
rustup component add rustc-dev llvm-tools --toolchain nightly-2026-08-24
cd tools/hash-rustc
cargo build --release
cargo check
cargo test
cargo clippy -p hash-rustc --all-targets --no-deps -- -D warnings
cargo fmt -p hash-rustc
LOOM_ITEM_HASHES=/tmp/items.json target/release/hash-rustc \
  --edition=2024 --crate-type=rlib /path/to/definition.rs
```

From the repository root, the five required identity checks are `cargo +nightly-2026-08-24 test --manifest-path tools/hash-rustc/Cargo.toml --test identity`.

Arguments go unchanged to `rustc_driver::run_compiler`. The driver's default sysroot is the compiler it was built against; an explicit `--sysroot` wins. The binary embeds an rpath to that compiler's libraries. Keep that toolchain installed while using the binary. Without `LOOM_ITEM_HASHES`, the callback does no hashing. With it, an old side output is removed before compilation and a new JSON document is written only after the compiler returns successfully. Version and help queries do not produce a document. A hashing or side-output error fails the command and names the problem. rustc's ordinary diagnostics and artifact production remain active.

The document has exactly these top-level fields:

```json
{
  "toolchain": "verbatim output of the pinned rustc -vV, including final newline",
  "items": {
    "entry": { "hash": "64 lowercase hex digits", "refs": ["helper"], "cycle": null },
    "helper": { "hash": "64 lowercase hex digits", "refs": [], "cycle": null }
  },
  "entry": { "entry": "same hash as items.entry.hash" }
}
```

Paths use `tcx.def_path_str`; a local root item can be named simply `entry`. `refs` is a sorted, deduplicated list of direct referents, including external referents. A cycle contains sorted local member paths; a self-recursive item has a one-member cycle. All supported definitions appear in `items`, including unreachable helpers. Closure bodies and anonymous constants are encoded inside their enclosing definition, rather than receiving artificial standalone entry identities.

Entries are public functions at the crate root and the original items annotated with Loom's `def` or `actor` attribute. Those procedural macros live in `crates/loom-guest-macros/src/lib.rs`. They consume the attributes, so discovery uses rustc's expansion provenance and the resolved macro DefId. Retained identifier spans distinguish the original item from generated wrappers. Qualified imports of the macro are supported. Source spans are used only for this entry selection, never as hash input.

## Identity rules

Each definition becomes an ordered, framed HIR stream, prefixed with `loom-hir-v1`. Node kinds, operators, literals, signatures, explicit generic arguments, bounds, pattern structure, and statement order are retained. Span offsets, hygiene numbers, HIR allocation IDs, local variable spellings, and the definition's own ordinary name are absent. Local uses encode the reverse position of their resolved binder in the lexical binding stack, so shadowed variables remain distinct. Generic parameters use declaration positions. Labels encode their resolved control-flow target's position. Generic bodies are hashed once, without monomorphization.

Named field and enum-variant spellings remain part of type structure. Representation options and resolved function attributes such as `track_caller`, target features, linkage, and instrumentation are included. Compiler optimization settings and the `inline`/`optimize` hints are not. Exported or imported symbol names are retained when the name is part of linkage. Inline assembly records its template, operands, options, and symbol referents without diagnostic spans.

An ordinary local reference contributes the referent's 32-byte hash. Constructors and variants contribute their structural position and the enclosing type's hash. Hashing proceeds from dependency sinks toward callers. For a strongly connected component, members are sorted by def path; internal references become member indices. Concatenate the length-framed member streams and BLAKE3-hash that cycle. Each member gets `blake3(cycle_hash || u64_le(member_index))`. Member paths select the order but are not themselves hashed. Identically structured cycles with the same relative member order therefore have identical member hashes, even in different modules.

An external reference is `blake3(crate_name_utf8 || u64_le(stable_crate_id) || DefPathHash_bytes)`. Here `stable_crate_id` is rustc's metadata-derived stable crate identity, not the dependency's source or rlib digest. Changing dependency bytes without changing that identity is outside this hash's guarantees. The build record must retain dependency and artifact identity separately.

Dot calls resolve through the body's `tcx.typeck(owner).type_dependent_def_id(call_hir_id)`. Inherent calls refer to the one selected inherent method. Trait calls refer to the trait method DefId, including explicit UFCS calls that initially identify an implementation method. **The implementation selected at monomorphization is not part of identity.** Overloaded operators expose their type-dependent referents through the same table. This is the requested generic-definition identity, not a proof that all executable instantiations behave identically.

## What changes a hash

Changing a literal, operator, signature, resolved callee, reachable helper, included representation, or included attribute changes that item's stream and propagates through its callers. Editing either member of a recursive cycle changes all its member hashes. Adding or editing an unreachable helper changes that helper's hash without changing an unrelated entry. Formatting, comments, local alpha-renaming, and declaration reordering do not change entry hashes.

The remaining over-approximations are specific: every HIR branch contributes even when its condition is statically false; merely referring to a function value creates an edge even if it is never called; explicit bounds and generic argument syntax contribute even when inference could recover them; literal kinds, suffixes, and string styles are retained; referencing an ADT includes all its fields and variants; field and variant names remain significant; included linkage and instrumentation flags may change identity without changing a particular invocation's result. Cycle renaming that changes lexical member order may change member hashes.

There are also exclusions, which must not be mistaken for over-approximations. Trait implementation selection, implicit drop glue, implicit coercion/adjustment machinery, linker inputs, global assembly, layout-randomization seeds, and runtime state are not transitively expanded. An actor marker selects its struct; trait-dispatched actor implementation bodies do not automatically become dependencies of that struct. Macro-expanded literals from `file!`, `line!`, or `include_str!` are real HIR content and can change hashes. These limits make the separate Wasm and toolchain identities necessary.

The initial encoder fails explicitly on delegated type inference, unsupported local referent kinds, and type-relative paths that lack a body type-checking resolution, including some shorthand associated types in item signatures. It does not substitute source text, unresolved names, or a whole-crate hash. Other unhandled forms must receive an encoder and a fixture before the supported surface is widened. Changing the encoder schema requires a new stream version and rehashing stored definitions. Cross-nightly hash compatibility is not promised.

## Seam for loom-build

This seam is specified here; it is not wired into storage or the build coordinator yet. Use the existing direct build path with `RUSTC=tools/hash-rustc/target/release/hash-rustc`, resolved to an absolute path before changing working directories. Set `LOOM_ITEM_HASHES` to a unique root-compilation output path. Dependency builds and concurrent crates must not share that path. Preserve `LOOM_RUSTC_ARGUMENTS`, working directory, environment, target, and outputs when replaying a captured recipe.

`direct.rs` currently has an isolated replay branch that selects `sysroot/bin/rustc`. Integration must preserve the configured driver there too; setting `RUSTC` alone does not establish this seam for every existing replay. Invalidate existing build-cache entries when adding the side-output contract, and require the JSON on cache hits. A successful root build without its requested JSON is an error, not permission to use the old identity.

After the build succeeds, validate the JSON, select the requested entry explicitly, and store the item-hash document in the CAS. Store item hashes and reference relationships as definition metadata. The current JSON contains digests and edges, not the canonical HIR preimages: its CAS key is the document-byte hash, not an individual item's HIR hash. A future per-item CAS encoding must export those preimages and the cycle objects; inserting unrelated JSON bytes under an item digest would violate the CAS contract.

Set `defs.behavior_hash` to the selected entry hash. Record the emitted Wasm's digest as `wasm_hash`; define `toolchain_hash` as BLAKE3 over length-framed fields containing the exact toolchain string, the verbatim `toolchain` text, the hash-rustc encoder/build identity, target, compiler arguments, and dependency/build-input identities. This distinguishes executable realizations that the HIR contract intentionally equates. Add `wasm_hash` and `toolchain_hash` to `code_changes`, and carry them through promotion, history, export/import, restore, and replay.

The current `defs` schema has no `behavior_hash` column, and the actor `code_changes` schema has neither new column. Migration must preserve existing records, distinguish old artifact-based identities from verified HIR identities, recompile and verify existing definitions where source is available, and only then retire the old identity path. This driver does not perform that migration.

## README summary

Loom can identify Rust definitions by resolved HIR content with `tools/hash-rustc`.
The driver uses the pinned `nightly-2026-08-24` compiler and its normal pipeline.
Set `LOOM_ITEM_HASHES` to receive item hashes, references, cycles, and entries as JSON.
Formatting, local renaming, and item reordering leave ordinary entry hashes unchanged.
Changing a reachable helper changes the entry; changing an unrelated helper does not.
Mutually recursive items share one cycle hash and receive indexed member hashes.
Inherent method calls resolve to one method; trait calls retain the trait method identity.
Generic bodies hash once, and monomorphized implementations are outside that identity.
Wasm and toolchain digests must separately identify each executable realization.
The loom-build and storage seam is documented above and is not yet connected.

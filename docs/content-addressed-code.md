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
LOOM_ITEM_HASHES=/tmp/items.json LOOM_ITEM_PREIMAGES=/tmp/item-preimages \
  target/release/hash-rustc \
  --edition=2024 --crate-type=rlib /path/to/definition.rs
```

From the repository root, the five required identity checks are `cargo +nightly-2026-08-24 test --manifest-path tools/hash-rustc/Cargo.toml --test identity`.

Arguments go unchanged to `rustc_driver::run_compiler`. The driver's default sysroot is the compiler it was built against; an explicit `--sysroot` wins. The binary embeds an rpath to that compiler's libraries. Keep that toolchain installed while using the binary. Without `LOOM_ITEM_HASHES`, the callback does no hashing. With it, `LOOM_ITEM_PREIMAGES=<directory>` is also required. An old JSON output is removed before compilation; the driver writes preimages after successful compilation and then publishes the JSON. Version and help queries do not produce a document. A hashing or side-output error fails the command and names the problem. rustc's ordinary diagnostics and artifact production remain active.

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

## Canonical encoding and checkable preimages

The existing `loom-hir-v1` byte encoding is unchanged. Files have no extension, compression, text conversion, or trailing newline. Each distinct item hash names `<LOOM_ITEM_PREIMAGES>/<hash>`, and hashing the complete file with BLAKE3 must produce that lowercase hexadecimal filename. Definitions with the same hash share one file. Existing identical files are reused; conflicting bytes fail the build without overwriting them. The directory retains immutable objects from previous builds; the successful JSON selects the current set. No directory is deleted during cleanup. A normal write failure removes its newly created partial file; an interrupted process can leave an incomplete file that a later build rejects.

All framing integers below are unsigned 64-bit little-endian, and all hash payloads are raw 32-byte BLAKE3 digests, never their hexadecimal spelling. An ordinary item is a sequence of tokens concatenated with no separator:

| Tag byte | Following bytes | Meaning |
|---|---|---|
| `00` | `u64_le(payload_length)`, then exactly that many payload bytes | Canonical HIR atom |
| `01` | `u64_le(member_index)` | Reference inside the current recursive component |
| `02` | 32 digest bytes | Reference to an already-hashed local item or an external referent |

The first token is atom `loom-hir-v1`: `00 0b 00 00 00 00 00 00 00` followed by its eleven UTF-8 bytes. Atoms are generated in the order specified by `tools/hash-rustc/src/encode.rs` and `src/encode/*.rs`. These source files are the normative v1 node schema. Structural traversal uses this pinned rustc's `rustc_hir::intravisit` walkers except where those encoders explicitly replace traversal. Another HIR producer must implement that schema, not use pretty-printed Rust or rustc's incremental hash.

In that schema, `text(value)` emits the UTF-8 bytes of `value`; `scalar(value)` emits its pinned Rust `Debug` representation as UTF-8; `tag(value)` emits two atoms, the pinned Rust `type_name` for the value's type and `Debug` of its enum discriminant, such as `Discriminant(0)`. `end()` is the atom `end`. Literal spellings and escapes therefore follow the pinned HIR literal `Debug` implementation. Lengths count bytes, not characters. This deliberately makes both the exact rustc version and encoder schema necessary to reproduce atoms from HIR. No locale-dependent formatting or source-span Debug output is used.

Binder uses are atoms `local-debruijn` and the decimal reverse binding-stack position. Generic uses are atoms `generic-position` and the decimal declaration position. References replace their original name tokens using the binary `01` or `02` token above. Metadata, signatures, delimiters, and node-specific atoms follow the encoder modules' explicit order; the schema includes every emitted atom even when two Rust syntaxes could have equivalent behavior. Constructor/variant adapters emit their kind and disambiguator atom, plus a variant-position payload where applicable, before the enclosing type reference. In v1 that variant payload uses the pinned host's `usize::to_le_bytes()` width (eight bytes on the verified aarch64 host); cross-host compatibility is not claimed.

Cycles require two levels because the original contract defines a member hash as `blake3(cycle_hash || index)`. Its item preimage is therefore exactly 40 bytes: the raw cycle digest followed by `u64_le(index)`. It cannot simultaneously be a standalone HIR stream while preserving that hash rule. The full normalized HIR is checkable through `<LOOM_ITEM_PREIMAGES>/cycles/<cycle_hash>`, whose bytes are `u64_le(length(member_0_stream)) || member_0_stream || ...`, in sorted def-path order, with no leading count or trailing bytes. Internal references in those streams use tag `01`; outgoing references use tag `02`. A self-recursive definition uses the same format with one member.

To verify a cyclic item independently, hash its 40-byte file, extract the first 32 bytes as the cycle key, hash the corresponding cycle file, decode its length-framed streams, and check that the final eight bytes select this item's index in the JSON `cycle` list. This exposes the whole recursive HIR preimage without changing any existing item identities. `preimage_rehashes_to_item_hash` checks both levels for mutual and self recursion as well as acyclic items.

## What changes a hash

Changing a literal, operator, signature, resolved callee, reachable helper, included representation, or included attribute changes that item's stream and propagates through its callers. Editing either member of a recursive cycle changes all its member hashes. Adding or editing an unreachable helper changes that helper's hash without changing an unrelated entry. Formatting, comments, local alpha-renaming, and declaration reordering do not change entry hashes.

The remaining over-approximations are specific: every HIR branch contributes even when its condition is statically false; merely referring to a function value creates an edge even if it is never called; explicit bounds and generic argument syntax contribute even when inference could recover them; literal kinds, suffixes, and string styles are retained; referencing an ADT includes all its fields and variants; field and variant names remain significant; included linkage and instrumentation flags may change identity without changing a particular invocation's result. Cycle renaming that changes lexical member order may change member hashes.

There are also exclusions, which must not be mistaken for over-approximations. Trait implementation selection, implicit drop glue, implicit coercion/adjustment machinery, linker inputs, global assembly, layout-randomization seeds, and runtime state are not transitively expanded. An actor marker selects its struct; trait-dispatched actor implementation bodies do not automatically become dependencies of that struct. Macro-expanded literals from `file!`, `line!`, or `include_str!` are real HIR content and can change hashes. These limits make the separate Wasm and toolchain identities necessary.

The initial encoder fails explicitly on delegated type inference, unsupported local referent kinds, and type-relative paths that lack a body type-checking resolution, including some shorthand associated types in item signatures. It does not substitute source text, unresolved names, or a whole-crate hash. Other unhandled forms must receive an encoder and a fixture before the supported surface is widened. Changing the encoder schema requires a new stream version and rehashing stored definitions. Cross-nightly hash compatibility is not promised.

## Seam for loom-build

This seam is specified here; it is not wired into storage or the build coordinator yet. Use the existing direct build path with `RUSTC=tools/hash-rustc/target/release/hash-rustc`, resolved to an absolute path before changing working directories. Set `LOOM_ITEM_HASHES` to a unique root-compilation output path and `LOOM_ITEM_PREIMAGES` to its preimage directory. Dependency builds and concurrent crates must not share that path. Preserve `LOOM_RUSTC_ARGUMENTS`, working directory, environment, target, and outputs when replaying a captured recipe.

`direct.rs` currently has an isolated replay branch that selects `sysroot/bin/rustc`. Integration must preserve the configured driver there too; setting `RUSTC` alone does not establish this seam for every existing replay. Invalidate existing build-cache entries when adding the side-output contract, and require the JSON on cache hits. A successful root build without its requested JSON is an error, not permission to use the old identity.

After the build succeeds, validate the JSON and rehash every referenced preimage, select the requested entry explicitly, and store the item preimages and cycle objects in the CAS under their verified hashes. Store item hashes and reference relationships as definition metadata. Store the JSON document under its own document-byte hash; inserting JSON bytes under an item digest would violate the CAS contract. The driver now exports all item preimages and cycle objects; wiring their verified ingestion into the existing CAS remains the integration task.

Set `defs.behavior_hash` to the selected entry hash. Record the emitted Wasm's digest as `wasm_hash`; define `toolchain_hash` as BLAKE3 over length-framed fields containing the exact toolchain string, the verbatim `toolchain` text, the hash-rustc encoder/build identity, target, compiler arguments, and dependency/build-input identities. This distinguishes executable realizations that the HIR contract intentionally equates. Add `wasm_hash` and `toolchain_hash` to `code_changes`, and carry them through promotion, history, export/import, restore, and replay.

The current `defs` schema has no `behavior_hash` column, and the actor `code_changes` schema has neither new column. Migration must preserve existing records, distinguish old artifact-based identities from verified HIR identities, recompile and verify existing definitions where source is available, and only then retire the old identity path. This driver does not perform that migration.

## Seam for loom-rt: a CAS-backed function compilation cache

This is a design for the lane that owns loom-rt; no runtime implementation is included here. The locally installed Wasmtime 48.0.1 source verifies `Config::enable_incremental_compilation(Arc<dyn CacheStore>) -> Result<&mut Config>` in `wasmtime-48.0.1/src/config.rs:418`. Enable both Cargo features `incremental-cache` and `cranelift`, keep the compiler enabled, select `Strategy::Cranelift`, and attach a shared store before constructing the engine. This caches native compilation results per Cranelift function across modules. Wasm validation, translation to Cranelift IR, and module assembly still occur.

Implement `wasmtime::CacheStore` on a named `LoomCompilationCache` struct holding the Loom store, a backend-build namespace, and thread-safe counters/error reporting. The trait requires `Send + Sync + Debug` and has synchronous methods `get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>>` and `insert(&self, key: &[u8], value: Vec<u8>) -> bool`. Return `Cow::Owned` for CAS-loaded blobs. Wasmtime can invoke these methods from parallel compiler workers; use the store's synchronized access and keep database locks outside compilation calls. Loom's existing `Store::put(kind, bytes)` and `Store::get(hash)` are synchronous, so this adapter need not block an async executor to await storage.

The lookup key comes from Cranelift, not rustc's incremental cache or Loom's item hash. Wasmtime's `CraneliftCacheStore` forwards the supplied bytes unchanged. In Cranelift 0.135.1, `compute_cache_key` hashes the `FunctionStencil`, ISA name, target triple, shared flags, and ISA-specific flag hash key through Rust's `Hash` trait into SHA-256, producing 32 bytes. The stencil includes a Cranelift version marker, signature, IR instructions and layout, stack/global definitions, relative source locations, and debug metadata. The function's own name and separate `FunctionParameters`, including the base source location and user external-name bindings, are outside the stencil. Cached `CompiledCodeStencil` data is deserialized and then has the current function parameters applied. This separation allows reuse across modules; equal Rust bodies or equal HIR hashes alone do not guarantee an equal stencil or cache key.

Treat the supplied key as opaque. Preserve its bytes in an index `(backend_build_namespace, cranelift_key) -> cas_blob_hash`. The namespace should identify the exact Wasmtime/Cranelift runtime build, including local patches and relevant feature configuration. It must not include the guest module hash, item name, or per-definition `toolchain_hash`, which would partition identical native function compilations. Cranelift already includes the target and compiler flags in its key. The runtime-build namespace additionally isolates patched builds that retain an upstream version number. Rustc's guest build remains represented by Loom's HIR/Wasm/toolchain records; those records do not replace this native cache key.

On `insert`, store the opaque serialized value through the CAS, for example with kind `cranelift-function`, then publish the lookup row using the returned content digest. A Cranelift key hashes compiler inputs, not the cached output bytes: putting the blob directly under that key would violate the CAS invariant. Verify an existing mapping before reusing it, and report a conflicting value instead of overwriting it silently. Store the blob before publishing the index row; an interrupted publication can leave an unreferenced CAS object for the normal CAS garbage collector. Keep cache-index rows as GC roots while live, and expire the index entry before allowing its blob to be collected. Concurrent equivalent inserts should converge on the same mapping.

On `get`, resolve the index, read and verify the CAS blob, and return it unchanged. Keep both the index and executable blobs within Loom's trusted compilation-cache boundary: a content digest verifies bytes, not that they are valid compiler output for the input key. A missing row is an ordinary cache miss. Storage errors, broken mappings, and corrupt blobs should increment distinct error counters and retain a diagnostic naming the key/blob. The trait cannot return an error from `get`; it can only return `None`, and `insert` can only return `false`. Wasmtime's adapter currently discards the insert boolean, while Cranelift recompiles after a missing or rejected blob. If Loom requires cache failures to fail a build, capture a compile-scoped error and return it from the existing runtime compilation boundary after Wasmtime returns. The adapter alone cannot enforce that policy.

A test named `shared_function_hits_cas_across_modules` should use a fresh temporary Loom CAS and a recording adapter:

1. Compile module A containing one exported function, such as `(func (export "f") (param i32) (result i32) local.get 0 i32.const 7 i32.add)`. Record lookup keys and inserts, and invoke `f(5)` to check that it returns 12.
2. Drop the module and engine. Construct another engine with the same configuration and persisted adapter. Compile module B with the identical function and a different custom section appended after the code section. Assert that A and B have different Wasm digests. Leave Wasmtime's whole-module cache unconfigured so a module-cache hit cannot bypass the tested path.
3. Require a B lookup for a function key inserted by A to return CAS bytes, with no replacement insert for that key, and invoke B's `f(5)` to check 12. Also capture Wasmtime's actual accepted-hit count: the internal Cranelift compiler logs `Incremental compilation cache stats` at trace level when the compiler is dropped. Drop all module/engine/config references that retain it before collecting that log. A `get` returning `Some` by itself is only a candidate hit because Cranelift can reject the blob.
4. Exclude trampoline hits from the verdict. Compile a control module C with the same single-function ABI but `i32.const 8` in a separate empty recording cache. Identify the body-specific key among A's inserts that is absent from C's inserts, and require B's successful lookup to include that key. Shared ABI trampolines may still hit when a body changes. Require C's function to return 13; if no body-specific key can be identified, fail the test instrument rather than claim reuse.
5. Add separate controls for an empty cache, changed target/compiler flags, corrupted CAS bytes, and an insert failure. Assert the documented miss/error policy and successful guest results where recompilation is allowed. Report candidate lookups, accepted compiler hits, inserts, and storage errors separately.

The API and key description above were checked against local registry sources: `wasmtime-environ-48.0.1/src/compile/mod.rs` (`CacheStore`), `wasmtime-internal-cranelift-48.0.1/src/compiler.rs` (forwarding, accepted-hit accounting, and drop-time statistics), and `cranelift-codegen-0.135.1/src/incremental_cache.rs` plus `src/ir/function.rs` (key, stencil, and parameter application). The cross-module test is a sketch, not an executed result. The loom-rt lane owns implementation and verification of this seam.

## README summary

Loom can identify Rust definitions by resolved HIR content with `tools/hash-rustc`.
The driver uses the pinned `nightly-2026-08-24` compiler and its normal pipeline.
Set `LOOM_ITEM_HASHES` and `LOOM_ITEM_PREIMAGES` for JSON identities and checkable bytes.
Formatting, local renaming, and item reordering leave ordinary entry hashes unchanged.
Changing a reachable helper changes the entry; changing an unrelated helper does not.
Mutually recursive items share one cycle hash and receive indexed member hashes.
Inherent method calls resolve to one method; trait calls retain the trait method identity.
Generic bodies hash once, and monomorphized implementations are outside that identity.
Wasm and toolchain digests must separately identify each executable realization.
The loom-build and storage seam is documented above and is not yet connected.

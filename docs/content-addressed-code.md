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

## Object cache

`hash-rustc` also wraps the installed LLVM backend when `LOOM_OBJECT_CACHE=<directory>` is set. It installs `CachingBackend` through `rustc_interface::Config::make_codegen_backend`; no backend dylib or `codegen-backends/` directory is needed. Item-hash JSON and preimages remain optional, independent side outputs. For native object caching on this pinned aarch64 Apple toolchain:

```sh
cd tools/hash-rustc
cargo build --release
LOOM_OBJECT_CACHE=/tmp/loom-objects LOOM_OBJECT_CACHE_STATS=1 \
  target/release/hash-rustc input.rs --edition=2024 --crate-type=rlib \
  -C opt-level=2 -C codegen-units=16 -C lto=off -C embed-bitcode=no
```

A hit skips LLVM IR generation, optimization, and object emission for that entire CGU. The wrapper restores its object with the current crate's symbols and lets rustc perform normal artifact production and linking. A miss delegates those operations to LLVM and publishes the completed object. Without the environment variable the original driver path remains active. `LOOM_OBJECT_CACHE_STATS=1` writes exactly one summary line to stderr after compilation:

```text
object-cache: cgus=<n> hits=<h> misses=<m> bytes_reused=<b>
```

CGUs bypassed for unsupported inputs count as misses. Diagnostic lines begin `object-cache: bypass`; only the summary begins `object-cache: cgus=`. Allocator shims and metadata are outside the CGU counters. Bytes reused measures the canonical cached object sizes.

### Key and supported code

The format is `loom-object-v1-macho-scalar`. Every field uses an unsigned 64-bit little-endian length followed by its bytes. The CGU key is BLAKE3 over the format, the compiler settings described below, the sorted mono-item fingerprints, and a canonical binding map for referenced symbols. A fingerprint starts with the existing definition's HIR content hash; a generic instance adds BLAKE3 hashes of the substituted structural scalar types, with lifetimes erased. It also includes `inline`/`optimize` attributes and the lowered MIR's scalar local types, statements, control flow, and resolved direct callees. A callee contributes its actual monomorphized implementation's fingerprint, including trait implementation bodies. CGU membership records linkage, visibility, and local-copy status. Spans, local variable names, crate names, and ordinary function symbol names do not enter this object identity.

The lowered-code refinement is necessary: the HIR identity above deliberately omits selected trait implementations, implicit adjustments, and optimization hints. Reusing objects under that identity alone could execute stale code. The existing HIR identity and JSON format are unchanged. Compiler-private debug representations used for lowered scalar operations and settings are covered by the exact compiler version in the key; they are not a cross-nightly interchange format.

Admission currently covers local ordinary functions with primitive scalar values or unit, primitive type instantiations, branches, and acyclic resolved direct calls to other admitted local functions. A CGU is cached only if every mono item has a supported identity. Static objects, global assembly, external monomorphizations, compiler shims, const generics, aggregate/reference/pointer layouts, indirect calls, recursive mono call graphs, implicit drop, inline assembly, and source-location-dependent panic or `track_caller` calls bypass with an explicit reason. Calls that MIR has already inlined are covered by the resulting lowered code. This is a conservative object-cache implementation, not a claim that a generic HIR hash identifies every Rust executable realization.

The settings include the exact pinned `rustc -vV` output, built-in target triple and complete target configuration, effective optimization/debug-assertion/overflow/panic settings, edition, path remapping, and `MACOSX_DEPLOYMENT_TARGET`, `SDKROOT`, and `SOURCE_DATE_EPOCH`. The supported explicit `-C` flags are:

```text
codegen-units       debug-assertions       debuginfo
embed-bitcode       extra-filename         force-frame-pointers
jump-tables         linker                 lto
metadata
no-redzone          no-vectorize-loops     no-vectorize-slp
opt-level           overflow-checks        panic
relocation-model    symbol-mangling-version
```

Every accepted flag's actual session value, including its default, enters the key. `-Z embed-metadata` is also admitted and keyed because the pinned Cargo emits it. Every other `-C` and `-Z` field must equal the pinned compiler default; exhaustive Rust struct destructuring checks that the audit covers every field. Explicit unlisted flags bypass even if their supplied value equals the default. Response files bypass because the raw argument guard cannot audit their expansion. No other `-Z` flags are admitted. Only built-in `aarch64-apple-darwin` is supported. LTO, incremental compilation, debug information, embedded bitcode, and requested assembly/LLVM-IR/bitcode outputs bypass. Set `-C lto=off` explicitly to avoid default local ThinLTO at optimized multi-CGU settings, and `-C embed-bitcode=no` to use the object-only store. These restrictions preserve the requested compiler behavior rather than silently altering its options.

### Symbols, publication, and the compiler boundary

Rust symbols normally include the crate identity. Before publication, the pinned toolchain's `llvm-objcopy` renames definitions and admitted local call references to canonical names derived from the cache key and binding position. On a hit it maps those names to the current compilation's symbols. Both the cache object and the original executable linkage therefore survive changing the crate name. The cross-crate tests use ordinary Rust-mangled public functions, not `no_mangle` fixtures. Functions are placed in distinct modules to obtain distinct CGUs; `#[inline(never)]` and the CGU count alone do not promise one CGU per function.

The directory contains `<key>.o`, `<key>.json` with the object digest/size and canonical symbol list, and `<key>.index.jsonl` with one JSON line containing `key`, `item_count`, `bytes`, and a Unix-seconds `created` timestamp. Per-entry index shards avoid concurrent appends to a shared index. Readers validate the full object digest, size, and symbol mapping. Invalid entries fail compilation. No global lock is used.

Publication writes and syncs a unique temporary file, then atomically hard-links its complete inode to the final name and removes the temporary name. This deliberately replaces the proposed rename: ordinary rename can overwrite a competing writer before comparing its bytes. Exclusive hard-link publication preserves the existing entry and rejects any different bytes for the same key. Metadata is published last and acts as the entry's commit marker. A crash can leave a temporary file or an uncommitted complete object, but cannot expose a partially written final object. `LOOM_OBJECT_CACHE_FAIL_PUBLISH=1` injects a fatal failure after the temporary object is complete and before final publication. Native concurrent-writer and failure-injection tests exercise this path.

The installed compiler differs from the proposed private-channel design. Its generic driver is `rustc_codegen_ssa::base::codegen_crate(backend, tcx)`. `CachedModuleCodegen`, `submit_post_lto_module_to_llvm`, and the coordinator field are `pub(crate)`, and `compile_codegen_unit` can return only a module plus cost. The wrapper implements `ExtraBackendMethods` and `WriteBackendMethods`, carrying a cached-object variant through the public scheduler; its optimization step does nothing and its emission step restores the object. It does not synthesize a `WorkProduct` or access the private `PostLto` channel. Unsupported sessions delegate to the stock LLVM backend. The pinned LLVM constructor returns a boxed trait object containing exactly `LlvmCodegenBackend`; the adapter's documented pointer conversion recovers that concrete allocation, with a zero-size assertion and native compiler-path tests.

### Verification and timing

Run the tests inside the tool crate so its toolchain pin applies:

```sh
cargo test
cargo clippy --all-targets --no-deps -- -D warnings
cargo build --release --bin hash-rustc
cargo run --release --example object_cache_measurement -- target/release/hash-rustc
```

The five required integration tests build fresh output directories, test cross-crate reuse and sibling invalidation, change optimization flags, and inject publication failure. They link and execute binaries against the generated rlibs. Additional tests cover unsupported flags, optimization hints, generic substitutions, selected trait implementations, symbol relocation, and concurrent publishers. The standalone measurement compiles the largest fixture, 48 separate function CGUs with 128 operations each, after changing one function. It reports the median of five alternating cached/uncached trials; setup and cache warming are outside the timed interval. There is no timing assertion.

On this machine, using the release driver and pinned toolchain, the five-run result was:

```text
object-cache measurement: fixture_modules=48 trials=5 median_with_cache_ms=456.571 median_without_cache_ms=387.252
```

Every measured cached compilation reported `cgus=48 hits=47 misses=1 bytes_reused=75952`. The cache was about 18% slower for this fixture despite skipping 47 LLVM codegen units. These wall times include frontend compilation, hashing, object relocation, and rlib production; they do not isolate the source of the overhead and do not establish a speedup.


### Real-crate measurements and default

The cache is **opt-in and inert by default**. With `LOOM_OBJECT_CACHE` unset, the driver uses the stock backend without cache hashing, eligibility probes, lookup, publication, object copying, or cache timers. Requesting the independent `LOOM_ITEM_HASHES` side output still performs its explicitly requested item hashing. `LOOM_OBJECT_CACHE_CALLS=1` exposes counters at the actual cache entry points. The unset-path regression checks zero hashing, lookup, store, publish, and object-copy calls; an enabled-cache positive control proves every counter observes real activity.

The real-crate runner replays captured Cargo invocations against isolated copies of the complete source. Dependencies are compiled once with the pinned nightly and held fixed outside the timed interval. Each of five trials warms the original crate into a fresh cache, changes one existing function, and compiles the changed crate with and without caching into separate fresh output directories. Trial order alternates. The SDK change adds one millisecond in `sleep` using `ms.saturating_add(1)`; the preview change replaces `intermediate\n` with `intermediate changed\n`. The native target and `-C opt-level=2` are used with the declared library crate types, `-C lto=off`, and `-C embed-bitcode=no`. Cargo's `-Z embed-metadata=no` and the machine's `-C linker=/usr/bin/cc` are preserved and included in the audited key.

```sh
cd tools/hash-rustc
cargo build --release --bin hash-rustc --example real_object_cache_measurement
mkdir -p target/real-object-cache-bench
target/release/examples/real_object_cache_measurement \
  target/release/hash-rustc target/real-object-cache-bench
```

The measured results on this machine were:

```text
object-cache real measurement: crate=loom_guest_rs opt_level=2 trials=5 cgus=16 hits=0 misses=16 bytes_reused=0 median_with_cache_ms=1041.367 median_without_cache_ms=1278.724 hashing_ms=7.304 llvm_ms=571.002 lookup_ms=0.000 object_copy_ms=0.000
object-cache real measurement: crate=loom_example_preview opt_level=2 trials=5 cgus=10 hits=0 misses=10 bytes_reused=0 median_with_cache_ms=541.253 median_without_cache_ms=549.417 hashing_ms=0.034 llvm_ms=306.433 lookup_ms=0.000 object_copy_ms=0.000
```

Both crates had **zero eligible CGUs**, because their units contain unsupported aggregate/pointer layouts, external monomorphizations, or compiler shims. These are measurements of enabled-cache eligibility checks followed by native compilation, not successful real-workload reuse. The SDK's lower cached median is not evidence of a cache speedup; its trials varied substantially. Hashing/eligibility did not dominate: its median was 7.304 ms for the SDK and 0.034 ms for the preview. Median LLVM time was 571.002 ms and 306.433 ms respectively; frontend work, monomorphization, linking, and process overhead account for the other work.

`LOOM_OBJECT_CACHE_TIMINGS=1` reports four phase durations. `hashing_ms` covers eligibility probes plus HIR/mono-item key construction when eligible; `lookup_ms` covers metadata/object reads and digest verification; `object_copy_ms` covers relocation and publication; `llvm_ms` sums elapsed delegated LLVM calls. Parallel worker durations and nested validation can overlap, so these counters are not an additive wall-time breakdown. In these two runs every CGU bypassed and LLVM used the stock backend. Each reported phase number is its own five-run median.

The SDK smoke test exposed eager HIR hashing of unsupported `Invocation::arguments` before CGU eligibility was known. The cache now checks lowered-code eligibility first and skips HIR hashing and store creation when no CGUs are eligible. A regression covers that exact `Self::Args` signature shape. The earlier 48-module micro-fixture remains 456.571 ms cached versus 387.252 ms uncached with 47 hits; the new real measurements establish the current coverage limit rather than a general performance verdict. The resulting default remains disabled.

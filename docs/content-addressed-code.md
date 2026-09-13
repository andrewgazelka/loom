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

From the repository root, the identity checks are `cargo +nightly-2026-08-24 test --manifest-path tools/hash-rustc/Cargo.toml --test identity`.

The driver forwards compiler arguments to `rustc_driver::run_compiler` and enables `-Zalways-encode-mir`. The driver's default sysroot is the compiler it was built against; an explicit `--sysroot` wins. The binary embeds an rpath to that compiler's libraries. Keep that toolchain installed while using the binary. Without `LOOM_ITEM_HASHES`, the callback does no hashing. With it, `LOOM_ITEM_PREIMAGES=<directory>` is also required. An old JSON output is removed before compilation; the driver writes preimages after successful compilation and then publishes the JSON. Version and help queries do not produce a document. A hashing or side-output error fails the command and names the problem. rustc's ordinary diagnostics and artifact production remain active.

The document has exactly these top-level fields:

```json
{
  "toolchain": "verbatim output of the pinned rustc -vV, including final newline",
  "items": {
    "entry": { "hash": "64 lowercase hex digits", "refs": ["helper"], "cycle": null },
    "helper": { "hash": "64 lowercase hex digits", "refs": [], "cycle": null }
  },
  "entry": { "entry": "same hash as items.entry.hash" },
  "effects": {
    "entries": { "entry": { "labels": [], "unknown": [] } },
    "instances": { "entry": { "labels": [], "unknown": [] } }
  },
  "schema": null
}
```

Paths use rustc's verbose disambiguated definition path, without a leading `::`, and with a crate prefix for external referents. Impl blocks and anonymous constants therefore retain their declaration disambiguators instead of overwriting another item's entry. A local root item can be named simply `entry`. `refs` is a sorted, deduplicated list of direct referents, including external referents. A cycle lists representative local paths in canonical content-class order; its current item's slot carries that item's own path. A self-recursive item has a one-member cycle. All supported definitions appear in `items`, including unreachable helpers. Closure bodies and anonymous constants are encoded inside their enclosing definition, rather than receiving artificial standalone entry identities.

Entries are ordinary public functions at the crate root, including `main`. Multiple root `pub fn` items are allowed. Private functions, nested public functions, and types are not entries. Guest source uses no macros or procedural attributes; the SDK has no guest macro crate. Entry discovery reads rustc's resolved item kind, parent, and visibility.

## Identity rules

Each definition becomes an ordered, framed HIR stream, prefixed with `loom-hir-v3`. Node kinds, operators, literals, signatures, explicit generic arguments, bounds, pattern structure, and statement order are retained. Span offsets, hygiene numbers, HIR allocation IDs, local variable spellings, and ordinary function names are absent. Local uses encode the reverse position of their resolved binder in the lexical binding stack, so shadowed variables remain distinct. Generic parameters use declaration positions. Labels encode their resolved control-flow target's position. Generic bodies are hashed once, without monomorphization.

Struct, enum, and union identity retains the item's own name (the last path segment), generics, fields or variants, and an unordered group of the existing item hashes of every local impl block whose self type resolves to that ADT. Both inherent and trait impls participate; type aliases in the self type are resolved. Impl streams encode their headers and an unordered group of member item references. Trait members also encode their declaration slot, so swapping implementations of two trait methods changes the hash. Dependencies inside a recursive component use the class-index convention below.

Renaming a **type** deliberately moves its hash and its users' hashes. Renaming a **function** or local does not: identical resolved function bodies sharing a hash is intentional Unison-style content semantics. The requested nominal token is the last segment, not a globally unique declaration identifier: equally named, equally defined types in different modules can still share a hash. This is not full Rust `TypeId` identity. Only direct ADT self types contribute impl dependencies; blanket impls and impls for `&Alpha` or `Box<Alpha>` do not become dependencies of `Alpha`. These limits do not widen object-cache admission.

Named field and enum-variant spellings remain part of type structure. Representation options and resolved function attributes such as `track_caller`, target features, linkage, and instrumentation are included. Compiler optimization settings and the `inline`/`optimize` hints are not. Exported or imported symbol names are retained when the name is part of linkage. Inline assembly records its template, operands, options, and symbol referents without diagnostic spans.

An ordinary local reference contributes the referent's 32-byte hash. Constructors and variants contribute their structural position and the enclosing type's hash. Hashing proceeds from dependency sinks toward callers. For a strongly connected component, content-based partition refinement assigns member classes: begin with one class, group lexicographically by the previous class index and encoded stream, and repeat until no class splits. Internal references use the previous round's class indices; outgoing references use resolved hashes. Equivalent recursive definitions share a class. Concatenate one length-framed stream per final class and BLAKE3-hash that cycle. Each member gets `blake3(cycle_hash || u64_le(class_index))`. No diagnostic path determines ordering or enters the hash. This also handles the cycles introduced by ADT → impl → method → Self dependencies without requiring a cryptographic fixed point.

An external reference is `blake3(u64_le(crate_name_utf8.len) || crate_name_utf8 || u128_le(crate_hash) || DefPathHash_bytes)`. The crate hash is rustc's strict version hash from `tcx.crate_hash`, not its stable crate ID. The fixed-metadata dependency regression changes the dependency body while preserving its name and metadata argument, and verifies that the referring item changes.

Dot calls resolve through the body's `tcx.typeck(owner).type_dependent_def_id(call_hir_id)`. Inherent calls refer to the one selected inherent method. Trait calls refer to the trait method DefId, including explicit UFCS calls that initially identify an implementation method. The implementation selected at monomorphization is not part of the generic function body identity. A reference to a concrete local ADT does include that ADT's direct impl dependencies, as specified above. Overloaded operators expose their type-dependent referents through the same table. This is the requested generic-definition identity, not a proof that all executable instantiations behave identically.

## Canonical encoding and checkable preimages

The current stream is `loom-hir-v3`. It replaces v2 to add ADT name tokens, local impl dependencies, unordered groups, and content-based recursive classes. Recompute v1/v2 identities from source when importing them; existing immutable preimages remain valid historical objects and must not be relabeled as v3. The object-cache admission gate and its `loom-object-v2-macho-mono` format are unchanged by this revision; changed local HIR identities naturally produce different keys. Files have no extension, compression, text conversion, or trailing newline. Each distinct item hash names `<LOOM_ITEM_PREIMAGES>/<hash>`, and hashing the complete file with BLAKE3 must produce that lowercase hexadecimal filename. Definitions with the same hash share one file. Existing identical files are reused; conflicting bytes fail the build without overwriting them. The directory retains immutable objects from previous builds; the successful JSON selects the current set. No directory is deleted during cleanup. A normal write failure removes its newly created partial file; an interrupted process can leave an incomplete file that a later build rejects.

All framing integers below are unsigned 64-bit little-endian, and all hash payloads are raw 32-byte BLAKE3 digests, never their hexadecimal spelling. An ordinary item is a sequence of tokens concatenated with no separator:

| Tag byte | Following bytes | Meaning |
|---|---|---|
| `00` | `u64_le(payload_length)`, then exactly that many payload bytes | Canonical HIR atom |
| `01` | `u64_le(member_index)` | Reference inside the current recursive component |
| `02` | 32 digest bytes | Reference to an already-hashed local item or an external referent |
| `03` | `u64_le(entry_count)`, then length-framed entry streams sorted lexicographically | Unordered dependency group, retaining duplicates |

The first token is atom `loom-hir-v3`: `00 0b 00 00 00 00 00 00 00` followed by its eleven UTF-8 bytes. Atoms are generated in the order specified by `tools/hash-rustc/src/encode.rs` and `src/encode/*.rs`. These source files are the normative v3 node schema, together with `src/graph/canonical.rs`. Structural traversal uses this pinned rustc's `rustc_hir::intravisit` walkers except where those encoders explicitly replace traversal. Another HIR producer must implement that schema, not use pretty-printed Rust or rustc's incremental hash.

In that schema, `text(value)` emits the UTF-8 bytes of `value`; `scalar(value)` emits its pinned Rust `Debug` representation as UTF-8; `tag(value)` emits two atoms, the pinned Rust `type_name` for the value's type and `Debug` of its enum discriminant, such as `Discriminant(0)`. `end()` is the atom `end`. Literal spellings and escapes therefore follow the pinned HIR literal `Debug` implementation. Lengths count bytes, not characters. This deliberately makes both the exact rustc version and encoder schema necessary to reproduce atoms from HIR. No locale-dependent formatting or source-span Debug output is used.

Binder uses are atoms `local-debruijn` and the decimal reverse binding-stack position. Generic uses are atoms `generic-position` and the decimal declaration position. References replace their original name tokens using the binary `01` or `02` token above. Metadata, signatures, delimiters, and node-specific atoms follow the encoder modules' explicit order; the schema includes every emitted atom even when two Rust syntaxes could have equivalent behavior. Constructor/variant adapters emit their kind and disambiguator atom, plus a variant-position payload where applicable, before the enclosing type reference. In v3 that variant payload uses the pinned host's `usize::to_le_bytes()` width (eight bytes on the verified aarch64 host); cross-host compatibility is not claimed.

Cycles require two levels because the original contract defines a member hash as `blake3(cycle_hash || index)`. Its item preimage is therefore exactly 40 bytes: the raw cycle digest followed by `u64_le(index)`. It cannot simultaneously be a standalone HIR stream while preserving that hash rule. The full normalized HIR is checkable through `<LOOM_ITEM_PREIMAGES>/cycles/<cycle_hash>`, whose bytes are `u64_le(length(member_0_stream)) || member_0_stream || ...`, in canonical content-class order, with no leading count or trailing bytes. Internal references in those streams use tag `01`; outgoing references use tag `02`. A self-recursive definition uses the same format with one member.

To verify a cyclic item independently, hash its 40-byte file, extract the first 32 bytes as the cycle key, hash the corresponding cycle file, decode its length-framed streams, and check that the final eight bytes select this item's index in the JSON `cycle` list. This exposes the whole recursive HIR preimage under the current schema. `preimage_rehashes_to_item_hash` checks both levels for mutual and self recursion as well as acyclic items.

## What changes a hash

Changing a literal, operator, signature, resolved callee, reachable helper, included representation, or included attribute changes that item's stream and propagates through its callers. Editing either member of a recursive cycle changes all its member hashes. Adding or editing an unreachable helper changes that helper's hash without changing an unrelated entry. Formatting, comments, local alpha-renaming, and ordinary declaration reordering do not change entry hashes. Associated types carry their zero-based position among their container's associated types: renaming a slot is free, but reordering slots can change identity. This distinguishes two otherwise identical unbounded associated types.

The remaining over-approximations are specific: every HIR branch contributes even when its condition is statically false; merely referring to a function value creates an edge even if it is never called; explicit bounds and generic argument syntax contribute even when inference could recover them; literal kinds, suffixes, and string styles are retained; referencing an ADT includes all its fields and variants; field and variant names remain significant; included linkage and instrumentation flags may change identity without changing a particular invocation's result. Function renaming does not change recursive class order or member hashes.

There are also exclusions, which must not be mistaken for over-approximations. Trait implementation selection, implicit drop glue, implicit coercion/adjustment machinery, linker inputs, global assembly, layout-randomization seeds, and runtime state are not transitively expanded. Struct identities include local impl blocks whose self type resolves directly to that struct. Blanket impl selection and impls for reference or wrapper self types are not expanded into the struct identity. Macro-expanded literals from `file!`, `line!`, or `include_str!` are real HIR content and can change hashes. These limits make the separate Wasm and toolchain identities necessary.

The encoder fails explicitly on delegated type inference, unsupported local referent kinds, and missing compiler resolution states. Associated-type shorthand in signatures and bodies is supported as described below. It does not substitute source text, unresolved names, or a whole-crate hash. Other unhandled forms must receive an encoder and a fixture before the supported surface is widened. Changing the encoder schema requires a new stream version and rehashing stored definitions. Cross-nightly hash compatibility is not promised.

## Integration with loom-build

`loom-build::identity` validates the compiler document and rehashes item and cycle
preimages before importing them into the CAS. The definition identity combines
its public entry identities. Stored metadata keeps the behavior hash, Wasm hash,
toolchain hash and item-document reference separate. `view` exposes these values.

`loom-api` compiles new definitions in a private store and publishes them through
`loom-store::commit_intake`. A graph update uses one private store for all affected
definitions, then `commit_update` publishes their names and session outcome in one
transaction. A failed caller leaves live names unchanged. Builder clones share
the compiler workspace lock because their filesystem artifacts are mutable even
when the definition stores are isolated.

This integration does not make HIR identity a complete executable identity. The
exclusions above still require separate Wasm and toolchain digests. See
[scripted updates](../examples/evolution/README.md) for propagation and recovery.

## Implemented loom-rt seam: a CAS-backed function compilation cache

`loom_rt::LoomCompilationCache` implements Wasmtime 48.0.1's synchronous
`CacheStore`. The core Wasm engine selects `Strategy::Cranelift` and installs the
adapter with `Config::enable_incremental_compilation`; Cargo enables
`incremental-cache` and `cranelift`. This caches native compilation per Cranelift
function across modules. Validation, translation to Cranelift IR, and module
assembly still occur. The whole-module cache is not enabled by this seam.

Cranelift supplies an opaque input key. Its function stencil, ISA, target and
compiler flags determine that key; it is not a Loom definition hash or a hash of
the cached output. `runtime_compilation_cache` maps
`(backend_namespace, compiler_key)` to the digest returned by
`Store::put("cranelift-function", bytes)`. Reads use `Store::get`, rehash the blob,
and return `Cow::Owned`. Existing mappings are verified; conflicting values
are reported rather than overwritten. Equivalent concurrent inserts converge.
The adapter uses synchronized store access without holding database locks over
compiler calls.

The backend namespace is computed when the engine is built, as BLAKE3 over
`Engine::precompile_compatibility_hash`: the Wasmtime version, the target triple,
and the compiler's flags and tunables. Wasmtime defines that hash as the condition
for one engine accepting another's serialized output, which is exactly what a
mapping asserts. `LoomCompilationCache::new` takes the configuration its engine
will use and hashes a probe engine built from it, because the hash is reachable
only from an `Engine` and an engine cannot be built after its cache is attached;
installing a cache store is not one of the hashed inputs. Package locations,
Cargo's resolved feature graph, and guest/module identities are not namespace
inputs. A same-version local patch to Wasmtime or Cranelift sources does not move
the namespace: nothing here patches them, and Cranelift refuses to deserialize a
value written by a different Cranelift version (its `VersionMarker` carries the
crate version), so such a value is rejected rather than replayed.

The blob is published before its index row. An interrupted publication can leave
an unreferenced object. The index's `ON DELETE RESTRICT` foreign key retains live
CAS blobs; `LoomCompilationCache::clear` expires the current backend's mappings
before those blobs may be collected. There is no automatic eviction or general
CAS garbage collector in this implementation. Native index writes and compiled
blobs remain trusted local compiler data: content verification is not proof that
arbitrary bytes are valid machine code for a compiler input.

`Runtime::compilation_cache_stats()` exposes candidate hits, misses, inserts,
error counts and the last diagnostic. Missing index rows are ordinary misses.
Storage failures, missing mapped blobs, corrupt blobs and conflicting inserts
have separate counters and diagnostics naming the key/blob. Wasmtime's trait
can return only `None` or `false` on failure, so the runtime compilation
boundary also checks the error counter before publishing a compiled module.
Any cache error during that window returns a retryable host error. Overlapping
compiles conservatively share failures; a later retry starts a fresh window and
can succeed after the underlying store is repaired. Diagnostics retain only
the latest error, keeping their memory bounded.

Run the native cross-module check with:

```sh
cargo test -p loom-rt --test compilation_cache -- --nocapture
```

`shared_function_hits_cas_across_modules` compiles A with `f(x) = x + 7`, closes
its engine and store, then compiles B with the same body and a different custom
section against the reopened CAS. A changed-body control C (`x + 8`) identifies
body-specific keys. B serves cached bytes only for those keys, forcing shared
ABI trampolines to miss. The test requires both a CAS candidate hit and a
Wasmtime-accepted compiler hit, no replacement insert for the body, different
A/B Wasm digests, and correct guest results (12, 12 and 13).
A compiler-flags control reuses the persisted cache with a different optimization
level and requires zero candidate or accepted hits with the same guest result.

The local run recorded one accepted body hit, two forced trampoline misses,
and zero storage errors. The adapter unit tests also verify conflicting inserts,
foreign-key retention and expiration, corrupt/missing blobs, and recovery after
a failed publication. Candidate hits and Wasmtime-accepted hits are reported
separately; returning `Some` from the adapter alone is not the success criterion.

## README summary

Loom can identify Rust definitions by resolved HIR content with `tools/hash-rustc`.
The driver uses the pinned `nightly-2026-08-24` compiler and its normal pipeline.
Set `LOOM_ITEM_HASHES` and `LOOM_ITEM_PREIMAGES` for JSON identities and checkable bytes.
Formatting, local renaming, and item reordering leave ordinary entry hashes unchanged.
Changing a reachable helper changes the entry; changing an unrelated helper does not.
ADT names and direct local impl blocks contribute to type identity; renaming a type propagates to its users.
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

The current format is `loom-object-v2-macho-mono`. Every field uses an unsigned 64-bit little-endian length followed by its bytes. The CGU key is BLAKE3 over the format, the compiler settings described below, the sorted mono-item fingerprints, and a canonical binding map for referenced symbols. A fingerprint starts with the existing definition's HIR content hash; a generic instance appends the canonical structural argument encoding described below, with lifetimes erased. It also includes `inline`/`optimize` attributes and the lowered MIR's scalar local types, statements, control flow, and resolved direct callees. A callee contributes its actual monomorphized implementation's fingerprint, including trait implementation bodies. CGU membership records linkage, visibility, and local-copy status. Spans, local variable names, and ordinary local function symbol names do not enter this object identity. External referents include their owning crate name and content hash. External object refinements also include the instantiating crate's stable identity, which fixes its unmapped relocation namespace.

The lowered-code refinement is necessary: the HIR identity above deliberately omits selected trait implementations, implicit adjustments, and optimization hints. Reusing objects under that identity alone could execute stale code. The HIR stream is v3; the item-document JSON shape is unchanged. Compiler-private debug representations used for lowered scalar operations and settings are covered by the exact compiler version in the key; they are not a cross-nightly interchange format.

Admission covers local ordinary functions with primitive scalar values or unit, primitive type instantiations, branches, and acyclic resolved direct calls to admitted functions. It also covers external instances and shims whose substitutions refer only to external definitions. A CGU is cached only if every mono item passes codegen admission. Static objects, global assembly, external instances with local substitutions, local compiler shims, local const generics, aggregate/reference/pointer local MIR, indirect calls, recursive mono call graphs, implicit local drop, inline assembly, and source-location-dependent local panic or `track_caller` calls bypass with an explicit reason. Calls that MIR has already inlined are covered by the resulting lowered code. This is a conservative object-cache implementation, not a claim that a generic HIR hash identifies every Rust executable realization.

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

Rust symbols normally include the crate identity. Before publication, the pinned toolchain's `llvm-objcopy` renames definitions and admitted local call references to canonical names derived from the cache key and binding position. On a hit it maps those names to the current compilation's symbols. Admitted local scalar objects can therefore survive changing the crate name. External objects retain unmapped generic-call relocations, so their keys include the instantiating crate's stable identity, including its metadata discriminator. They can reuse after a one-function edit within that namespace; a crate rename makes them miss. A renamed-crate external aggregate regression first reproduced undefined linker symbols without this field and now links and executes successfully. The cross-crate tests use ordinary Rust-mangled public functions, not `no_mangle` fixtures. Functions are placed in distinct modules to obtain distinct CGUs; `#[inline(never)]` and the CGU count alone do not promise one CGU per function.

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

The cache is **opt-in and inert by default**. With `LOOM_OBJECT_CACHE` unset, the driver uses the stock backend without cache hashing, eligibility probes, lookup, publication, object copying, or cache timers. The independent `LOOM_ITEM_HASHES` and `LOOM_ITEM_COVERAGE` diagnostics still perform their explicitly requested hashing or coverage audit; neither is enabled by default. `LOOM_OBJECT_CACHE_CALLS=1` exposes counters at the actual cache entry points. The unset-path regression checks zero hashing, lookup, store, publish, and object-copy calls; an enabled-cache positive control proves every counter observes real activity.

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

## Coverage gap

The following inventory and counts describe the pre-coverage baseline. The current encoder and cache admission rules follow in “Coverage closure”. At baseline, the HIR item encoder and the cache's monomorphized-item encoder had different admission rules. `LOOM_ITEM_COVERAGE=<report.json>` audits both explicitly and continues ordinary compilation. It records every directly refused item and its first refusal reason, instead of stopping at the first rejected CGU. The ordinary hashing path still fails on unsupported input; the audit does not publish incomplete item hashes or cache objects.

The HIR encoder's complete explicit refusal inventory (`encode.rs`, `encode/{items,types,expressions,visitor}.rs`, and `graph.rs`) is:

- Type-relative paths outside a typechecked body, including associated-type projections such as `Self::Args` in signatures.
- Delegated type inference (`TyKind::InferDelegation`), error types, error expressions, and error patterns.
- Unbound local references; unresolved HIR paths; `Self` aliases whose owner is not an impl item; method calls missing a `type_dependent_def_id`; unresolved loop targets or targets absent from the encoder's active target stack. These include invalid or missing compiler-resolution states, not blanket refusals of methods, loops, or `Self`.
- HIR owner nodes other than an item, foreign item, impl item, or trait item (`item kind`); free items other than functions, constants, statics, type aliases, structs, unions, enums, traits, or trait aliases (`non-definition item`). The graph selects local functions/associated functions, constants/associated constants, statics, type aliases/associated types, structs, enums, unions, traits, and trait aliases. Other local definition kinds are excluded as independent candidates; constructors, variants, and fields are expanded through their parent. A reference to any other excluded local definition is refused (`reference to unsupported`).

The cache applies these additional restrictions in `object_cache.rs`, even when the HIR item can be encoded:

- Static and global-assembly mono items require a relocation identity.
- External monomorphizations and all compiler-generated instance kinds other than `InstanceKind::Item` require an identity. This includes compiler shims.
- Const generic arguments require a canonical value identity. Type generic arguments and all instantiated MIR local types must be `bool`, `char`, signed/unsigned integers, floats, `!`, or unit. All remaining types are refused, including references, raw pointers, function types, arrays, slices, nonempty tuples, structs, enums, unions, closures, coroutines, trait objects, and unresolved/alias types, because layout, allocation, or drop identity is absent. Consequently standard types such as `String`, `Vec`, `Option`, `Result`, and `serde_json::Value` are refused here even though their HIR syntax is supported.
- Recursive direct-call graphs, or an active call chain reaching 64 instances, require a cycle identity.
- Indirect calls require callable identity; direct callees with late-bound arguments, failed resolution, or unresolved instances are refused. Calls to `#[track_caller]` functions require source-location identity. Refusals in a callee also refuse its caller.
- MIR terminators other than `Return`, `Goto`, `SwitchInt`, `Unreachable`, `UnwindResume`, and supported direct `Call` require additional codegen identity. On this pinned compiler the excluded variants are `UnwindTerminate`, `Drop`, `TailCall`, `Assert`, `Yield`, `CoroutineDrop`, `FalseEdge`, `FalseUnwind`, and `InlineAsm`.
- During actual cache-key construction, a local function absent from the HIR document is refused (`no HIR identity`). The mono audit runs the preliminary admission check before constructing that document, so its counts do not include downstream HIR-identity failures.

The counts below use a fresh staged copy of the merged source tree, native `aarch64-apple-darwin`, the pinned nightly, and `-C opt-level=2 -C lto=off -C embed-bitcode=no`. These are the post-merge coverage results; the historical timing measurements above used the earlier source snapshot. HIR candidates are the selected local definitions, including generated definitions; “encoded” means the individual encoder and reference-shape check succeeded, not that its transitive hash graph is complete. Each directly refused definition is counted once, without counting its dependent definitions as additional HIR refusals. Mono counts cover unique `MonoItem` values, including external instances generated in the target crate; placements separately count the same instance each time it appears in a CGU. Every mono item is checked, including siblings after a refusal.

| Crate | HIR candidates | HIR encoded | HIR refused | CGUs | Unique mono items | Refused unique mono items | Mono placements | Refused placements |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `crates/loom-guest-rs` | 209 | 164 | 45 | 16 | 1,438 | 1,438 | 1,853 | 1,853 |
| `examples/rust-preview` | 16 | 16 | 0 | 10 | 1,055 | 1,055 | 1,322 | 1,322 |

All 45 SDK HIR refusals are `type-relative path outside body`. Among unique mono items, the SDK has 1,364 first refusals for external monomorphization/compiler shims and 74 for unsupported types. The preview has 1,036 external/shim refusals and 19 type refusals. The preview therefore demonstrates the distinction directly: every selected HIR definition encodes, while every emitted mono item fails cache admission.

```text
loom_guest_rs item-coverage: candidates=209 encoded=164 refused=45
loom_guest_rs mono-coverage: cgus=16 unique_items=1438 refused_unique_items=1438 placements=1853 refused_placements=1853
loom_example_preview item-coverage: candidates=16 encoded=16 refused=0
loom_example_preview mono-coverage: cgus=10 unique_items=1055 refused_unique_items=1055 placements=1322 refused_placements=1322
```

Reproduce with a fresh staging directory from inside the tool crate:

```sh
cd tools/hash-rustc
cargo build --bin hash-rustc --example real_object_cache_measurement
coverage_stage=$(mktemp -d "$PWD/target/item-coverage.XXXXXX")
target/debug/examples/real_object_cache_measurement \
  target/debug/hash-rustc "$coverage_stage" coverage
```

This builds dependencies once, captures Cargo's real root-crate compiler invocations, and audits both root crates without enabling the object cache. Reusing a staging directory in coverage mode is rejected to prevent mixing source snapshots. Each `<crate>-coverage.json` contains the individual refused item names and reasons; `<crate>-coverage-output/stderr.log` preserves compiler diagnostics. Diagnostic runs are separate from the timing samples.


## Coverage closure

The diagnostic now constructs actual mono identities through `src/mono.rs`, the same canonical encoder used by the object cache. Its `mono.hashes` map contains BLAKE3 digests of successful preimages. Hashability and object-cache eligibility are separate: a complete mono identity does not by itself describe all selected implementations, relocations, or source-location-sensitive generated code. The earlier baseline mono numbers measured cache admission, before there was a structural mono encoder.

### Canonical additions

- Associated-type shorthand such as `Self::Args` and `T::Item` emits the resolved associated definition, the structural receiver type, and explicit generic arguments. Body resolution uses rustc's type-dependent table where it has an entry; type nodes without that entry use `rustc_hir_analysis::lower_ty` and its resolved alias definition. This covers signatures and Serde-generated body annotations. No unresolved spelling or source-text hash is substituted. Associated-type definitions include their positional slot as described above.
- An ordinary function instance hashes as `blake3(generic_definition_hash_bytes || canonical_generic_args)`. The definition digest is raw 32-byte BLAKE3. External definitions use the framed crate name, strict crate hash, and DefPathHash encoding above. Constructors, closures, coroutines, and other nested definitions without an independent HIR owner use a tagged nested-definition digest containing their kind, lexical disambiguator, enclosing definition's content hash, and a variant index where applicable. Their enclosing HIR contains the nested body.
- Every mono argument stream starts with the framed atom `generic-args-v1` and the framed decimal argument count. Each argument carries a kind atom. Lifetimes emit `erased-region`; types recursively encode their variant and fields; constants encode their evaluated type and value tree. All atoms use an unsigned 64-bit little-endian byte length followed by payload bytes. Enum discriminants and scalar metadata use the pinned compiler's Debug representation; names, allocator IDs, and diagnostic spans are absent.
- Structural type encoding covers primitives, references and raw pointers with mutability, arrays with their evaluated length, slices, strings, tuples, ADTs with referent hashes and recursive arguments, function items, function pointers with signature/ABI/safety and binder kinds, trait objects with trait/projection referents and arguments, closures, coroutines and witnesses, foreign types, and unsafe binders. Parameter and bound-variable positions are structural. ADTs refer to their definition hash instead of recursively expanding fields, so recursive types terminate. Higher-ranked binder spellings are discarded.
- Const values use typed value trees. A leaf records its byte width and 128-bit little-endian scalar bits. A branch records its child count and recursively encoded typed children. Unevaluated consts remain refusals.
- Compiler-generated instances use a synthetic generic-definition digest containing their referent, instance/shim kind, and every kind-specific field: virtual slot, reification reason, callable/drop/clone type, closure referent, track-caller bit, coroutine receiver mode, or future-drop types. Their substitutions then use the same argument encoder. Static mono items use a `static` tag and the HIR referent digest.

The remaining explicit HIR refusals are delegated inference, error nodes, unresolved paths/methods/locals/control-flow targets, unsupported owner or referent kinds, type-relative non-type paths without body resolution, and type-relative type paths without a resolved projection. Remaining mono refusals are global assembly without an independent HIR identity, missing local referent identity, unevaluated consts, alias/inference/error/placeholder types, and pattern types. **Each category has count zero in all three audited crates.** These states require a defined canonical representation before admission; the encoder returns the named refusal and publishes no substitute hash.

### Before and after

The baseline used a preserved copy of the original driver. Both runs replayed the same Cargo invocations and staged source at native `aarch64-apple-darwin`, `-C opt-level=2 -C lto=off -C embed-bitcode=no`. The actor dependencies were built with the pinned nightly on this Mac. Its existing `recursion_depth_exceeding_limit` warning is preserved in stderr; compilation and both audits exited successfully.

Before:

```text
loom_guest_rs item-coverage: candidates=209 encoded=164 refused=45
loom_guest_rs mono-coverage: cgus=16 unique_items=1438 refused_unique_items=1438 placements=1853 refused_placements=1853
loom_example_preview item-coverage: candidates=16 encoded=16 refused=0
loom_example_preview mono-coverage: cgus=10 unique_items=1055 refused_unique_items=1055 placements=1322 refused_placements=1322
loom_actor item-coverage: candidates=1195 encoded=906 refused=289
loom_actor mono-coverage: cgus=16 unique_items=11087 refused_unique_items=11087 placements=19707 refused_placements=19707
```

After:

```text
loom_guest_rs item-coverage: candidates=209 encoded=209 refused=0
loom_guest_rs mono-coverage: cgus=16 unique_items=1438 refused_unique_items=0 placements=1853 refused_placements=0
loom_example_preview item-coverage: candidates=16 encoded=16 refused=0
loom_example_preview mono-coverage: cgus=10 unique_items=1055 refused_unique_items=0 placements=1322 refused_placements=0
loom_actor item-coverage: candidates=1195 encoded=1195 refused=0
loom_actor mono-coverage: cgus=16 unique_items=11087 refused_unique_items=0 placements=19707 refused_placements=0
```

Fresh coverage mode now stages and audits all three crates. `audit` mode reuses a captured stage for encoder-only changes without rebuilding dependencies:

```sh
cd tools/hash-rustc
cargo build --release --bin hash-rustc --example real_object_cache_measurement
coverage_stage=$(mktemp -d "$PWD/target/item-coverage.XXXXXX")
target/release/examples/real_object_cache_measurement target/release/hash-rustc "$coverage_stage" coverage
target/release/examples/real_object_cache_measurement target/release/hash-rustc "$coverage_stage" audit
```

Reuse `audit` only while the staged source and dependency artifacts are unchanged. Source changes require a fresh stage. The optional `LOOM_BENCH_CRATE` filter selects one captured crate.

### Object-cache eligibility and measurements

External instances, including shims, are eligible when their entire canonical identity refers only to external definitions. Their dependency crate hashes cover the compiled definitions; substitutions containing local types or closures remain ineligible because they can select local implementations. Their object refinements also include the instantiating crate's stable identity to protect unmapped external generic-call relocations. This restriction is only in the object key, not the canonical mono hash. Local functions retain the explicit scalar-MIR and resolved-callee admission checks. Static/global-assembly units, aggregate local MIR, unresolved/indirect calls, recursive call graphs, track-caller calls, and unsupported terminators still bypass with a reason. A mono hash is never treated as sufficient evidence to admit those objects.

The native regressions execute code from reused external aggregate objects after a one-function edit and verify that a changed local Drop implementation cannot restore stale external generic code. Existing cross-crate symbol relocation and selected-trait-implementation regressions still pass. The default remains opt-in.

The five-trial measurements below use the same one-function edits and alternating trial order as the earlier measurements, with the release driver:

```text
object-cache real measurement: crate=loom_guest_rs opt_level=2 trials=5 cgus=16 hits=6 misses=10 bytes_reused=156080 median_with_cache_ms=883.364 median_without_cache_ms=1120.808 hashing_ms=21.644 llvm_ms=2153.142 lookup_ms=2.171 object_copy_ms=206.083
object-cache real measurement: crate=loom_example_preview opt_level=2 trials=5 cgus=10 hits=6 misses=4 bytes_reused=194192 median_with_cache_ms=590.386 median_without_cache_ms=709.234 hashing_ms=12.558 llvm_ms=735.839 lookup_ms=1.932 object_copy_ms=172.896
```

Both second builds reused six CGUs in every trial: 6/16 for the SDK and 6/10 for preview. These medians are about 21% and 17% lower with caching, respectively. Other workloads were active on this Mac, and earlier trials varied substantially; the result establishes real object reuse and these observed medians, not a general speedup. The cache remains disabled by default.

The coverage-closure suite had 62 passing tests, including structural mono substitutions, associated-type slots, external dependency content invalidation, and native relocation/drop regressions. `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` also pass.


### Nominal ADT identity revision

The v3 revision adds type-name tokens and direct local impl dependencies without changing object-cache admission. The required Alpha/Beta tests cover structs, enums, and unions; adding Drop changes Alpha and its instances while leaving Beta unchanged. Other regressions check inherent and trait impl body edits, impl reordering, function/local renaming, trait member slot binding, and recursive content equivalence. The preimage regression includes shared recursive classes and multiple anonymous constants with distinct diagnostic labels.

The final suite has 71 passing tests. `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all --check` pass from `tools/hash-rustc`.

The audit below replays the same three captured source trees and dependencies before and after this revision. HIR candidate counts increase because impl blocks now receive independent item hashes. Mono item counts remain unchanged.

Before this revision:

```text
loom_guest_rs item-coverage: candidates=209 encoded=209 refused=0
loom_guest_rs mono-coverage: cgus=16 unique_items=1438 refused_unique_items=0 placements=1853 refused_placements=0
loom_example_preview item-coverage: candidates=16 encoded=16 refused=0
loom_example_preview mono-coverage: cgus=10 unique_items=1055 refused_unique_items=0 placements=1322 refused_placements=0
loom_actor item-coverage: candidates=1195 encoded=1195 refused=0
loom_actor mono-coverage: cgus=16 unique_items=11087 refused_unique_items=0 placements=19707 refused_placements=0
```

After this revision:

```text
loom_guest_rs item-coverage: candidates=251 encoded=251 refused=0
loom_guest_rs mono-coverage: cgus=16 unique_items=1438 refused_unique_items=0 placements=1853 refused_placements=0
loom_example_preview item-coverage: candidates=18 encoded=18 refused=0
loom_example_preview mono-coverage: cgus=10 unique_items=1055 refused_unique_items=0 placements=1322 refused_placements=0
loom_actor item-coverage: candidates=1571 encoded=1571 refused=0
loom_actor mono-coverage: cgus=16 unique_items=11087 refused_unique_items=0 placements=19707 refused_placements=0
```

The opt-level 2 measurements use five trials per crate, the same one-function edits, fresh per-trial caches, and alternating cached/uncached build order:

```text
object-cache real measurement: crate=loom_guest_rs opt_level=2 trials=5 cgus=16 hits=6 misses=10 bytes_reused=156080 median_with_cache_ms=13741.390 median_without_cache_ms=11686.786 hashing_ms=208.752 llvm_ms=24220.112 lookup_ms=4.436 object_copy_ms=1044.311
object-cache real measurement: crate=loom_example_preview opt_level=2 trials=5 cgus=10 hits=6 misses=4 bytes_reused=194192 median_with_cache_ms=6804.953 median_without_cache_ms=8033.357 hashing_ms=220.238 llvm_ms=11981.402 lookup_ms=10.405 object_copy_ms=613.174
```

Every second build reused six CGUs: 6/16 for the SDK and 6/10 for preview. The SDK's cached median was higher; preview's was lower. Concurrent workloads were active on this Mac and trial times varied substantially, so these results establish successful reuse and the observed medians, not a general speedup or an isolated encoder performance comparison. The timing samples ran without concurrent tests or builds from this worktree. The admission gate and opt-in default remain unchanged.


## Effect rows

The compiler driver emits residual host effects from the resolved call graph. A row contains the labels that can reach the host, which is the outermost handler. Concrete trait and generic calls contribute only their selected callees. The driver unions the entry's concrete instance rows to produce its entry row.

The driver recognizes one effect primitive by its resolved definition path, `loom_guest_rs::perform`. The package name is `loom-guest-rs`; rustc uses the crate name `loom_guest_rs`. Wrappers such as `sleep`, `now`, and `fs::list` are ordinary functions: their rows come from the literal or evaluated constant passed to `perform` in their bodies. Import aliases preserve the resolved definition identity. Handler functions retain their special subtraction semantics.

The driver enables `-Zalways-encode-mir` on every crate it compiles so callers can analyze external non-generic function bodies. Dependencies compiled with stock rustc must also enable this flag, for example through the build's `RUSTFLAGS`. Missing MIR for a user dependency is an explicit compiler error; it cannot establish an empty effect row.

The driver's JSON adds `effects` and `schema` without changing the existing identity keys. The effect contract is:

```json
{
  "effects": {
    "entries": {
      "guest::main": { "labels": ["sleep"], "unknown": [] }
    },
    "instances": {
      "guest::main": { "labels": ["sleep"], "unknown": [] }
    }
  }
}
```

`entries` keys name entry items; `instances` keys identify concrete instances. Each row has `labels` and `unknown`. A literal or rustc-evaluated const passed to `perform` adds its label. A dynamic label stops compilation at its call site.

A total `handle([labels], handler, body)` subtracts those labels from the body's row and includes the handler callback's own residual effects. `handle_any` can forward, so it subtracts nothing. For a pinned `handle_with("<hash>", body)`, the build supplies CAS handler metadata through `LOOM_HANDLER_ROWS`:

```json
{
  "<64-character handler hash>": {
    "labels": ["fs.read"],
    "unknown": [],
    "handled": ["sleep"]
  }
}
```

`labels` and `unknown` are the stored handler's residual row. Optional `handled` labels describe its total-handling contract; omission subtracts nothing from the body's row. The driver includes the stored residual row and subtracts `handled` from the callback row. Missing metadata for a referenced hash is a compiler error. Source lowering records the pinned dependency and emits the internal `handle_pinned("<hash>", dependency::handle, body)` helper to preserve the hash through resolution.

`loom-check` has one effect-analysis path. Source checking discovers root public functions and leaves effect rows pending with `unknown = true`. It does not infer rows or evaluate constants from Rust syntax. The build integration must pass the complete driver JSON to `CheckedDef::apply_driver_effects_json` before admitting executable code. The method requires an entry row for every export and rejects missing or ambiguous rows. It accepts an exact entry name or a unique qualified name ending in that export's name. The build must invoke the driver and finalize these rows before admitting executable code.

Each entry hash includes a tagged reference to the exported `LOOM_SCHEMA` constant when present. Changing the schema changes the entry identity; unrelated constants remain outside that identity.

Finalization installs exactly the inferred row. A dynamic call site produces `effect label at <span> is not a literal or const; rows are inferred and need a static label`. The checker also rejects unknown sites in supplied driver rows. Sandboxing is omission: total handlers remove labels from the residual row, and the existing host enforcement refuses effects outside that row.


### Behavior schema constant

A crate-root `pub const LOOM_SCHEMA: &str = "...";` declares the behavior schema. Rustc evaluates the constant and the driver emits its string value in the new top-level `schema` field. The field is `null` when no schema constant is present. The build must generate the existing `loom_schema` ABI wrapper from that value; behavior loading reads the wrapper. A Rust constant alone does not create a Wasm export.

The build must generate the entry ABI from root public functions and the existing `loom_schema` ABI from the evaluated `schema` field. `loom-behavior` reads that generated export; guest constants require no attributes or macro expansion.
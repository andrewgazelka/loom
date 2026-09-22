# Bundles: `export` and `import`

A bundle moves definitions between Loom nodes as one file. It carries every
byte the receiving node needs to rebuild the definitions from source and the
identities the rebuild must reproduce. It never carries compiled wasm: the
receiver compiles, and a rebuilt definition whose hash differs from the
recorded one refuses the whole bundle. That refusal is the compatibility check
between the two nodes' toolchains.

Produced by the `export` verb, consumed by the `import` verb, on every
transport (CLI, `POST /v1/command`, MCP). The CLI writes and reads the file;
HTTP and MCP callers move the bytes through the CAS: `export` returns
`result.bundle.$ref`, fetched with `GET /v1/cas/{cid}`; `import` takes the
`$ref` returned by `POST /v1/cas` (`application/octet-stream`).

## Container: CARv1

The file is a [CARv1](https://ipld.io/specs/transport/car/carv1/) archive with
BLAKE3-256 CIDs. Byte layout, in order:

| offset | bytes | meaning |
| --- | --- | --- |
| 0 | `varint(len(H))` | unsigned LEB128 length of the header, minimal encoding |
| | `H` | DAG-CBOR map `{"roots": [CID], "version": 1}`; exactly one root, no other keys |
| | `varint(len(C_i) + len(B_i))` | length of the following CID and block bytes |
| | `C_i` | binary CIDv1: `0x01`, codec varint, multihash `0x1e 0x20` + 32 digest bytes |
| | `B_i` | the block bytes |
| | ... | repeated for every block, no trailing bytes |

The unsigned varint is at most nine bytes and carries at most 57 bits: eight
full seven-bit groups plus a ninth byte whose value may only be 0 or 1. A
longer value, a non-minimal encoding (a terminating zero group) or a
zero-length frame is rejected. `C_i` must
re-encode to exactly the bytes read. Only two codecs occur: `0x55` (raw) and
`0x71` (DAG-CBOR); every DAG-CBOR block must decode as canonical DAG-CBOR.
Every block's BLAKE3-256 must equal the digest in its CID; every CID appears
once. The first block after the header is the root named in the header.

CIDs printed in JSON (`result.root`, `result.bundle.$ref`) use the base32
multibase string form of the same binary CID.

## Block order

1. The root block.
2. One definition record per definition in the closure, ascending by
   definition hash.
3. Every CAS object the root lists, ascending by CID string.

Equal inputs produce byte-identical bundles; the exporter's CAS timestamps
are not included.

## Root block (DAG-CBOR, `loom_bundle: 1`)

```
{
  "loom_bundle": 1,
  "roots":       { <name>: <definition hash hex> , ... },
  "definitions": { <definition hash hex>: <link to record block>, ... },
  "objects":     [ { "cid": <link>, "kind": <CAS kind string> }, ... ]
}
```

`roots` are the names `import` binds (under `--into prefix/` when given); each
value is the current hash of that name on the exporter. `definitions` holds
the whole transitive `deps` closure of the roots, not just the roots. A link
is a DAG-CBOR CID (tag 42); the JSON rendering is `{"$ref": "<cid>"}`.
`objects` lists every non-record block with the `kind` the exporter's CAS
stored it under; the importer stores each object under that kind, leaving any
object it already has untouched. Unknown keys anywhere are rejected.

## Definition record (DAG-CBOR, `loom_definition: 1`)

```
{
  "loom_definition": 1,
  "hash":            <definition hash hex>,
  "lang":            "rust" | "javascript" | "typescript",
  "names":           [ <current names of this hash on the exporter> ],
  "source":          <link, raw>,           // stored source, byte for byte
  "deps":            { <alias>: <definition hash hex>, ... },
  "allowed_effects": [ <label>, ... ] | null,
  "sig":             <TypeSig: exports and effect row>,
  "identity":        null | {
      "behavior_hash":  <hex>,               // equals "hash" for Rust
      "wasm_hash":      <hex>,               // identity only; the wasm is not included
      "toolchain_hash": <hex>,               // named in a mismatch error
      "item_hashes":    <link, raw>          // the compiler's item document
  },
  "preimages":   [ <link, raw>, ... ],       // item preimages, cycle preimages, entry root
  "trees":       [ <link, DAG-CBOR>, ... ],  // crate and vendor trees, see below
  "preparation": null | { "key": <hex>, "overlay": <link, DAG-CBOR> }
}
```

`source` is the exact stored source: a plain Rust file or the JSON
`{"files": {...}}` source bundle, unchanged. `deps` are the resolved hashes
the definition was admitted with; every value is a key of `definitions`.
`identity` is `null` for V8 scripts, whose hash is a function of source,
policy and engine ABI and needs no compiler document.

## Objects included

For every definition in the closure:

| object | codec | kind on the exporter | why |
| --- | --- | --- | --- |
| source | raw | `source_bundle` | rebuild input |
| item document (`identity.item_hashes`) | raw | `item-hashes` | identity the rebuild must reproduce; entry name to item hash |
| item preimages, when stored | raw | `item-preimage` | the hashed compiler items behind each entry |
| cycle preimages, when stored | raw | `item-preimage` | 40-byte cycle members named by items with `cycle` |
| entry root preimage (the definition hash itself), when stored | raw | `entry-root` | sorted entry name/hash pairs whose BLAKE3 is the definition hash |
| `[loom.crates]` pins from the source's `Cargo.toml` | DAG-CBOR tree + raw blobs | `tree`, `blob` | `loom-crates/<hash>` sources materialized by `preparation::materialize_tree` |
| `loom.vendor-tree` in the source bundle, when present | DAG-CBOR tree + raw blobs | `tree`, `blob` | vendored dependencies of isolated builds |
| resolver overlay (`preparation.overlay`), when this node resolved it | DAG-CBOR | `rust-prepared-dependencies` | `Cargo.lock` and, for isolated builds, the `loom.vendor-tree` hash |
| the overlay's vendor tree, when present | DAG-CBOR tree + raw blobs | `tree`, `blob` | vendored dependencies the overlay points at |

Trees are walked to closure: every link inside a DAG-CBOR object is included
recursively (subtrees and file blobs), so a receiver materializes them
without any network access.

`preparation.key` is the exporter's `rust_preparations` row key, a BLAKE3 over
the manifest, lock, dependency pins, SDK fingerprint and isolation flag. It is
a claim, not an instruction: the importer derives the key for the record
itself, from the bundled source and the staged dependencies, and refuses the
bundle when the two differ (the error names both keys and the definition
hash). Only that locally derived key is ever seeded, without overriding a key
the receiver already resolved. The overlay may hold only `Cargo.lock` and
`loom.vendor-tree`, and the vendor tree it names must be one of the record's
`trees`, so every crate the overlay points at arrives as hashed bundle bytes.
A receiver whose SDK fingerprint differs therefore refuses the bundle at this
step rather than resolving the lock itself: a resolver row is trusted only
when this node would have computed it.

## Objects excluded, on purpose

| object | reason |
| --- | --- |
| compiled wasm (`component_hash`, `wasm_hash` bytes) | import rebuilds from source; a hash mismatch is the compatibility signal, not a silent import of foreign binaries |
| build logs (`logs_ref`) | exporter-local |
| `defined` and `component_built` events, name history, timestamps | the receiver writes its own records; the bundle carries facts, not the exporter's ledger |
| observed effects, effect results, traces, actor state | runtime history, not definition content |
| `javascript_definition` / `javascript_source` objects | recomputed by the receiver's V8 admission from `source` |

## Import semantics

1. Resolve `bundle` to a raw CAS object; read the bytes.
2. Verify the container as described above. Decode the root; it may list at
   most 1024 definitions and 1,000,000 objects (the same caps `export`
   applies), checked before any record is decoded. Every `definitions` link
   and `objects` entry must be a block of the bundle, every link inside a
   record must be a listed object, and no block may be unreferenced. Every
   listed object must be reached from some record, directly or through the
   link closure of a DAG-CBOR tree; an object nothing reaches refuses the
   bundle, naming it. A Rust record carries `identity`, a script record does
   not. A record's `preparation` overlay must be DAG-CBOR holding only
   `Cargo.lock` and `loom.vendor-tree`, and that vendor tree must be among
   the record's `trees`. Every `deps` value must be a bundled definition;
   the dependency order is computed without recursion and a cycle is
   rejected. Every root name passes the one name rule (non-empty, no
   whitespace or control characters, no leading `#`, not 64 hex, no empty
   path segment); `--into` passes the same rule.
3. Under the definitions gate, read the live names. For each root name
   (prefixed by `--into`), an existing binding to a different hash is an
   error; a binding to the same hash is a no-op.
4. Snapshot the live store into a private staged store. Store every object
   under its kind.
5. For each definition in dependency order: when the record carries a
   `preparation`, stage its overlay and derive the preparation key locally
   for the record; a key different from the bundle's refuses the import,
   naming both keys and the definition hash. Then rebuild the definition
   through the same admission path as `add` (check, prepare, build, publish)
   into the staged store. A rebuilt hash different from the record's `hash`
   fails the import; the error names both hashes and both toolchain hashes.
6. Commit every publication and its build event into the live store in one
   SQLite transaction, after checking that the live names still equal the
   snapshot from step 3. A concurrent binding aborts the transaction; nothing
   partial is ever visible.

The failure window is therefore empty: until step 6 commits, the live store
has not changed. Objects and resolver rows written to the staged store,
including an overlay staged under a key that then failed the check in step
5, are discarded with it.

## Export semantics

`export <targets>...`: a target is a definition name (its current hash is the
root) or a definition hash that is the current value of at least one name
(every such name becomes a root). A hash no name currently points at is
rejected, because an import binds names. The closure follows `deps`
transitively, up to 1024 definitions and 1,000,000 objects or 512 MiB, the raw
upload limit. The bundle bytes are stored as a raw CAS object of kind
`bundle`, which is why `export` needs the define scope, like `POST /v1/cas`:
a read token cannot write up to 512 MiB into the live CAS. The CLI claims
`--out` before sending the request (an existing file refuses the export),
downloads into `<out>.tmp`, and renames it into place, so `--out` is either
absent or the complete bundle.

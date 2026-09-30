# skin-weld through Loom, end to end (the engine's first check)

The game engine (rgb, 2026-09-30) asked for `skin_weld::weld(head, body, params)` as the first real operation:
pure, deterministic, plain data in and out. This runs the engine's own `skin-weld` crate as a wasm cell on a Loom
daemon and compares it with the in-process native call.

No `skin-weld` source is stored here. `assemble.py` reads `crates/skin-weld/src` from the rgb checkout at run
time, inlines its modules into one `lib.rs`, strips the tests and the `thiserror` derive, hides the crate's own
root `pub fn`s, rewrites the two `dyn Fn` sites (below), and appends `glue.rs`: a packed-bytes entry
`weld_blobs(head: StoreRef, body: StoreRef, params: Vec<f64>) -> Result<StoreRef, String>`.

Data path: the engine's meshes are little-endian arrays in a container (`u32 magic, u32 sections, u64 length per
section, sections padded to 8`). Inputs are uploaded with `POST /v1/blob`; the cell reads them with
`loom::kernel::get`, welds, writes the result with `loom::kernel::put` and returns a `StoreRef`; the client
downloads it with `GET /v1/blob/{hash}`. No float travels as JSON or as DAG-CBOR numbers.

## The step the engine's gate runs (2026-09-30)

`loom/definitions/skin-weld/lib.rs` in rgb is what `loom export-dir` writes from the daemon, and the daemon's source is
`assemble.py`'s output, so the committed file can go stale when `crates/skin-weld/src` changes. The gate asks:

```sh
python3 assemble.py --rgb <rgb checkout> --check <rgb>/loom/definitions/skin-weld/lib.rs    # rc 0 current, 1 stale, 2 cannot assemble
```

Exit 1 prints the first differing lines and the regeneration steps (`assemble.py --out guest.rs`, `loom update skin-weld
guest.rs --manifest Cargo.toml --lock Cargo.lock`, `loom export-dir loom/definitions skin-weld`). Exit 2 means one of the
asserted rewrites (the two `dyn Fn` sites, the `thiserror` derive) no longer matches: the engine's source changed in a way
the assembler must learn. `--rgb` defaults to `$RGB_DIR`.

Reproduce:

```sh
cd native && cargo build --release --offline && ./target/release/weld-native ../out   # reference + packed inputs
python3 assemble.py                                                                   # out/guest.rs
python3 run.py            # uploads, evals on a daemon at 127.0.0.1:8850, compares byte for byte (--slow: opt-level 0)
```

`native/` builds the engine's synthetic neck (the constants and builders of `skin-weld/src/tests.rs`, a
48-column by 40-row tube) and runs the in-process `weld` for the expected output.

## Result (2026-09-30, this Mac, machine load 35 to 45, one run each, not repeated on a quiet machine)

* **Output is bit-exact** against the in-process result: both skins (positions, normals, uvs, joints, weights,
  triangles, the morph target), the four index maps, and all 17 report numbers. Same-machine only; the engine
  does not need more.
* Input: head 1,177 vertices and 2,256 triangles, body 1,226 and 2,352 (107 KB and 97 KB); welded head 834 and
  body 1,177 vertices; result blob 197 KB.
* **Native in-process `weld`: 2.8 ms.** Wasm, optimized profile (opt-level 2): **5.0 ms per call** (median of
  15, including reading 204 KB and writing 197 KB through the store), about 1.8x native. Wasm at the default
  `eval` profile (opt-level 0): 26.7 ms, 9.5x. The engine's own figure was 1.0 to 1.9x for optimized wasm.
* Build: 12.5 s cold for the 2,282-line crate at opt-level 2 (2.5 s at opt-level 0), 0.2 s when cached.
* Upload of 107 KB to `/v1/blob`: 1 ms (the first call 20 to 40 ms).

## What it found

* **`dyn Fn` is refused.** Loom infers effect rows by resolving every call statically, so a call through
  `&dyn Fn(..)` fails with "effect analysis cannot resolve this callable". skin-weld has two (a closure table in
  `weld`, a callback parameter in `bridge::loop_of`); `assemble.py` rewrites them to a bool switch and `impl Fn`.
  Real engine code will hit this again; the honest options are rewriting such sites or letting an unresolved
  call widen the row to `unknown` where the caller's policy allows (not built).
* **`thiserror` and any derive are refused in guests** (no procedural macros), so error enums need a manual
  `Display`.
* **An entry that returns a byte string cannot be answered as JSON.** `StoreRef` is therefore text (hex hash and
  length); bulk data belongs in blobs, not in replies.
* **Diagnostics count the compiler's text, not yours** (comments and blank lines are dropped, statements
  re-wrapped); `eval` returns `line_map` on success, but a compile error's line numbers are in the compiler's text.
  This is how "cannot resolve this callable at line 2152" pointed at a line that did not exist in the source.

## Real size: the forge's MetaHuman head and body (2026-09-30)

The engine's `RGB_WELD_DUMP` of the `character_weld` job (`/Volumes/Projects/tmp/weld-dump`, packed arrays, not in
this repo): head 17,918 vertices and 34,960 triangles, body 49,209 and 87,436, nine `Params` as f64, and the
in-process result. `native/` has a `--dump` mode that loads it, runs the in-process `weld` seven times and writes
the packed containers; `run.py` (pointed at that output) uploads them and runs the same guest.

* **All 12 output arrays (head and body: positions, normals, uvs, joints, weights, triangles) are bit-identical to
  the engine's own `weld_out.*` files**, and the 17 report numbers and the four index maps match the native run.
  Same machine, morph targets not dumped. Result blob 5.8 MB from 5.2 MB of inputs.
* Timing, at machine load 30 to 40, not repeated on a quiet machine: in-process native `weld` median 146.8 ms (min
  111.1 ms, 7 runs, load 40); wasm at the standard profile median 111.5 ms (min 106.2 ms, 15 runs, load 30 to 37),
  including reading 5.2 MB and writing 5.8 MB through the store; wasm at the interactive profile 598 ms. The two
  medians were taken minutes apart under different load, so read them as "the same order of magnitude", not as wasm
  beating native.
* Build: 17 s cold for the crate at the standard profile.

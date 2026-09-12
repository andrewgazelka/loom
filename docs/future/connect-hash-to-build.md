# Connect hash-rustc to loom-build

The seam is documented in docs/content-addressed-code.md: run `hash-rustc` as `RUSTC` in loom-build's direct mode, read `LOOM_ITEM_HASHES`, store item hashes and preimages in the CAS, set `defs.behavior_hash` to the entry hash, and add `wasm_hash` and `toolchain_hash` columns to `code_changes` (loom-actor) so a promote row names the executable realization, not only the source identity.

Done when: renaming a local or reformatting a Loom definition leaves its behavior hash unchanged (test through the full build); `actor_lineage` shows which item hashes moved between two promotes; the wasmtime cache key and `wasm_hash` agree.

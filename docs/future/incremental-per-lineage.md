# Incremental compilation per definition lineage

rustc reuses MIR and codegen per item when `-C incremental` points at a persistent directory keyed by the same HIR fingerprints hash-rustc uses. Give each definition lineage (the chain of code_changes rows) its own incremental directory under the build cache so a one-function candidate recompiles one function.

Done when: median wall time of a one-function change on `examples/rust-preview` is measured before and after (5 runs), and build-std is confirmed paid once per toolchain.

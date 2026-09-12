# Object cache on by default

The content-addressed object cache in hash-rustc is opt-in (`LOOM_OBJECT_CACHE`). Measured today (hydra, 5 trials, opt-level 2): guest SDK 883 ms cached vs 1121 ms uncached; rust-preview 590 vs 709 ms; 48-module micro fixture 457 vs 387 ms (slower). Admission is narrower than hashability.

Done when: admission covers the mono items that dominate LLVM time, the micro fixture no longer regresses (or is documented as the floor), and the default flips with the measurement table updated.

# Deterministic replay for pure definitions

Determinism is not a runtime promise (docs/research/determinism.md). Pure definitions (no host effects) are deterministic by construction; an opt-in mode could assert bit-identical replay for them across hosts (NaN canonicalization on, relaxed SIMD off, fuel in the toolchain hash are already free).

Done when: a pure definition's output bytes are identical on macOS and Linux for the same input, asserted by a test that runs both.

# SFI on Cranelift (option, not plan)

Prior art exists (relon-codegen-cranelift, Lightweight Fault Isolation): sandbox native code from `rustc_codegen_cranelift` with an SFI pass and a verifier, skipping the wasm stage. Costs: owning the instrumentation, the verifier, a platform layer without libc, native fuel/preemption and stack switching, per-arch artifacts. See docs/research/sandboxing.md.

Trigger: measured per-candidate compile time or guest CPU overhead above a bound the owner sets. Until then: not started.

# Sandboxing: why wasm stays, and what SFI on Cranelift would cost

## What makes the wasmtime sandbox a sandbox (read: docs.wasmtime.dev/security; bytecodealliance.org "Security and Correctness in Wasmtime", 2022-09-13)

1. A discipline in generated code: all guest memory access relative to one linear memory with
   bounds checks or guard pages; indirect calls through typed tables; the only exits are the
   imports we provide; stack limits; catchable traps.
2. A checker that does not trust the compiler: wasm bytecode validation before compilation,
   plus translation validation of Cranelift's output (VeriWasm, NDSS 2021: proves memory access
   and control flow cannot escape for all inputs). Advisory GHSA-jhxm-h53p-jm7w (aarch64
   miscompile that escaped) is why the checker exists. CFI work is ongoing.

Cranelift is a code generator; the sandbox is the wasm target semantics plus validation.

## SFI without wasm (read today)

- `relon-codegen-cranelift` (docs.rs): enforces "the same four hard sandbox guarantees the
  wasm-AOT backend ships, inside Cranelift IR" (beta).
- Lightweight Fault Isolation, Stanford 2024: SFI on native code with a verifier; competitive
  with or faster than wasm on their benchmarks.
- Native Client (Google): the earlier native SFI; rewritten for x86-64; last LLVM patches 2015;
  wasm supplanted it (Gobi paper, arXiv 1912.02285).

Recipe if ever wanted: rustc to Cranelift IR, an SFI pass over memory ops and indirect calls, a
custom platform layer with no libc/syscalls, a verifier over emitted machine code, native
implementations of fuel/epoch preemption and stack switching. Payoff: no LLVM-to-wasm step and
possibly less bounds-check overhead. Cost: owning all of the above, per-arch artifacts.

## Decision

wasm is the checkable form of exactly the properties wanted, maintained by others with formal
translation validation. Guest behaviors are wasm; trusted code (runtime, daemon, tools) is
native. Layering: tenant VM (outer) -> wasm instance per actor (inner) -> capabilities. SFI on
Cranelift stays an option with a trigger: measured per-candidate compile time or guest CPU
overhead above a stated bound.

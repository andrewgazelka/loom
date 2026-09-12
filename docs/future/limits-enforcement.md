# Fuel and memory limits per actor

`memory_max` and `fuel` are recorded meta keys; enforcement lands with wasm behaviors: wasmtime fuel and memory limits per instance, classified as deterministic traps (poison), while wall-clock epoch preemption is a retry.

Done when: a behavior that spins is poisoned with reason `fuel` after the configured budget; a behavior that allocates past `memory_max` traps; both limits are part of `toolchain_hash` so replay sees the same budget.

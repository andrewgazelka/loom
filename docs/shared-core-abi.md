# Shared-core guest ABI

Rust core definitions use `--cfg loom_core` and one imported shared memory
per execution. The backend rebuilds `wasm32-unknown-unknown` standard libraries
with atomics and immediate-abort panics. The stock `wasm32-wasip1-threads`
SDK probe emitted pthread waits outside initialization, so it is rejected. Siblings are trusted code in
one execution, not security boundaries. Component definitions retain their WIT ABI.

All pointers and lengths are unsigned wasm32 values. Packed byte results use
pointer in low 32 bits and length in high 32 bits. Byte results are canonical
DAG-CBOR envelopes `{ok: value}` or `{error: string}`. The receiver owns returned
allocations and calls `loom_dealloc(ptr,len,1)`. Input slices are borrowed only
for the duration of the call. Host effect requests are copied and strictly
validated before scheduling, identity, or recording.

Imports from `loom`:

- `perform(ptr:i32,len:i32)->i64`: suspend this Store, return copied response.
- `fork(fn:i32,data:i32)->i64`: schedule exactly one task; zero refuses without
  starting a task. Nonzero IDs are execution-local and never reused.
- `join(id:i64)->i32`: zero means task completed and all its writes are visible.
  Nonzero means failure; guest aborts the whole execution. The host must stop
  and drain all workers before freeing execution memory. Failure cannot resume
  arbitrary borrowed Rust state after a sibling trap.

Exports:

- `loom_alloc(size:i32,align:i32)->i32`, zero on allocation failure;
  `loom_dealloc(ptr:i32,size:i32,align:i32)` with the original layout.
- `loom_call(args_ptr:i32,args_len:i32)->i64`.
- `loom_init()->i64`.
- `loom_run(state_ptr:i32,state_len:i32,msg_ptr:i32,msg_len:i32)->i64`.
- `loom_fold(state_ptr:i32,state_len:i32,event_ptr:i32,event_len:i32)->i64`.
- `loom_task_run(fn:i32,data:i32)` invokes the SDK task trampoline.
- Mutable `__stack_pointer`, `__wasm_init_tls`, `__tls_size`, `__tls_align`.

Each Store requires a distinct stack and TLS allocation through `loom_alloc`.
The host initializes TLS before entering guest Rust. Zero-sized TLS needs no
allocation. Shared module initialization must complete once before worker
execution; linker initialization waits are only allowed in this verified
startup stage. The builder accepts only the known LLVM initializer shape and
replaces its contended wait with a trap, removing its now-unused notification.
It rejects waits or notifications everywhere else. No worker may rerun active
data initialization over live state.

Scoped closures and results stay in shared memory. `F: Send` and `T: Send` plus
an invariant lexical scope lifetime prevent borrowed values from escaping.
The scope object stays on its owner; children can create nested scopes.
The owning scope joins every registered task, including forgotten handles,
before reclaiming task storage. Results publish with release/acquire atomics.
Allocator locking uses only short spin critical sections with no effects,
imports, suspension, or atomic waits. Runtime memory maximum bounds the heap.

The builder exports mutable per-instance `__loom_stack_low` and
`__loom_stack_high`. Root bounds come from the linker's `__stack_low` and
initial `__stack_pointer`; worker bounds come from their exact stack allocation.
The host sets both bounds before setting a worker's stack pointer or initializing
TLS. Every guest write to `__stack_pointer` traps unless the unsigned value lies
inside those bounds, including both endpoints. The compiler disables red zones
so a frame cannot access memory below the stack pointer before this check.

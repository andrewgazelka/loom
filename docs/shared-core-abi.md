# Shared-core guest ABI

Rust definitions use one imported shared memory
per execution. The backend rebuilds `wasm32-unknown-unknown` standard libraries
with atomics and immediate-abort panics. The stock `wasm32-wasip1-threads`
SDK probe emitted pthread waits outside initialization, so it is rejected. Siblings are trusted code in
one execution, not security boundaries.

All pointers and lengths are unsigned wasm32 values. Packed byte results use
pointer in low 32 bits and length in high 32 bits. Except for `join_error`, byte
results are canonical DAG-CBOR envelopes `{ok: value}` or `{error: string}`. The receiver owns returned
allocations and calls `loom_dealloc(ptr,len,1)`. Input slices are borrowed only
for the duration of the call. Host effect requests are copied and strictly
validated before scheduling, identity, or recording.

The `spawn` and `join` core imports implement scoped and detached closures;
they are not root effects. Scoped jobs must finish before entry returns.
Detached jobs still running when `loom_call` returns are cancelled without
an implicit join.

Imports from `loom`:

- `perform(ptr:i32,len:i32)->i64`: suspend this Store, return copied response.
- `spawn(fn:i32,data:i32,detached:i32)->i64`: schedule exactly one task;
  `detached` is 0 for scoped or 1 for detached; other values trap. Zero refuses
  without starting a task. Nonzero IDs are execution-local and never reused.
- `join(id:i64)->i32`: zero means task completed and all its writes are visible.
  Scoped jobs can only be joined by their spawning fiber; a scoped failure traps
  and cancels the execution before borrowed Rust state can resume. Detached jobs
  can be joined by any fiber in the same execution. Their task failure returns
  1 without cancelling the execution; an unjoined detached failure is discarded.
  The host stops and drains all workers before freeing execution memory.
- `join_error(id:i64)->i64`: retrieve a failed detached job's full
  `shared job {scope}: ...` diagnostic as packed UTF-8 bytes, without a CBOR
  envelope. The receiver owns this allocation and frees it with alignment 1.
  Unknown, scoped, unfinished, and successful jobs trap.

Exports:

- `loom_alloc(size:i32,align:i32)->i32`, zero on allocation failure;
  `loom_dealloc(ptr:i32,size:i32,align:i32)` with the original layout.
- `loom_call(args_ptr:i32,args_len:i32)->i64`.
- Optional `loom_schema()->i64`, returning the behavior schema SQL.
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
Detached closures and results require `Send + 'static`. Their closure slot,
result slot, and publication flag share one reference-counted allocation.
The task and handle each own a reference; dropping an unjoined handle leaves
the task running, and the last reference frees the storage. Guest traps and
execution cancellation do not run guest destructors; execution memory teardown
reclaims allocations left behind by those paths.
Allocator locking uses only short spin critical sections with no effects,
imports, suspension, or atomic waits. Runtime memory maximum bounds the heap.

The builder exports mutable per-instance `__loom_stack_low` and
`__loom_stack_high`. Root bounds come from the linker's `__stack_low` and
initial `__stack_pointer`; worker bounds come from their exact stack allocation.
The host sets both bounds before setting a worker's stack pointer or initializing
TLS. Every guest write to `__stack_pointer` traps unless the unsigned value lies
inside those bounds, including both endpoints. The compiler disables red zones
so a frame cannot access memory below the stack pointer before this check.

## Guest handler ABI v2

Core artifacts now carry `core-handlers-v2`. Previous `core-shared-v1`
artifacts fail admission and must rebuild from their stored source. Component
artifacts retain their separate protocol marker.

Additional imports from `loom`:

- `handle_push(function:u32,data:u32,labels_ptr:u32,labels_len:u32)->u64`:
  install a frame; zero refuses. Labels are canonical DAG-CBOR: null matches
  every label and permits forwarding; an array is a total handler for those
  labels and rejects `Forward` for a matching effect.
- `handle_pop(frame:u64)->i32`: remove the top frame after inherited children
  and active callbacks drain. Zero succeeds; failure must abort before borrowed
  handler storage can be reclaimed.
- `resume(k:u64,ptr:u32,len:u32)->i32`: consume a one-shot continuation and
  supply a canonical DAG-CBOR value. Input bytes remain borrowed for the call.
- `abandon(k:u64)->i32`: consume a continuation and abort its performer.
- `continuation_drop(k:u64)->i32`: report an unconsumed deferred continuation;
  the performer receives `continuation dropped`. This distinct import preserves
  the diagnostic difference between explicit abandonment and an accidental drop.

Additional exports:

- `loom_handler_run(function:u32,data:u32,k:u64,op_ptr:u32,op_len:u32)->u64`:
  invoke a handler on a separate instance with its own stack and TLS, over the
  execution's shared memory. Return packed canonical DAG-CBOR
  `{resume: value}`, `{forward: null}`, or `{deferred: null}`. The host releases
  input and returned buffers with their original allocation layouts.
- `loom_effect_run(ptr:u32,len:u32)->u64`: perform an effect from an
  independently instantiated scheduler child. The host owns the input and
  frees the packed response after consumption.

An execution can retain up to eight successful callback instances for reuse.
Checkout resets TLS, stack bounds and pointer, epoch deadline, occurrence keys,
error context, and the outer handler stack. Check-in releases frame references
and scheduling permits. Trapped or cancelled callbacks are discarded; execution
drain releases the cache before execution memory is reclaimed.

Dispatch walks the execution scope's frames from innermost to outermost.
Handlers are deep: the performer's remaining computation retains its handler
stack. The callback itself runs with the stack below its own frame, so its
effects cannot recursively enter that frame. A mutable callback has at most
one active invocation, enforced by an asynchronous host lock, never a guest
spinlock held across an effect.

Scoped and detached children inherit the stack at spawn time. A lexical
handler's removal still waits for every inheriting child, including detached
children, because handler callbacks can borrow its owner's stack. This is the
handler's lifetime obligation; entry return adds no detached join. A trap in a
borrowed handler callback still cancels the execution, even when its performer
is detached. Cross-definition calls
start a separate execution memory and do not inherit guest frames.
Cancellation drains children and callbacks before frame data and borrowed
parent storage. Handler traps abort the execution and identify the frame.
Only effects reaching the outermost host handler participate in recording
and replay. Pure execution may install guest handlers; root-bound effects
remain forbidden.

# Guest-defined algebraic effects

---

Loom: real algebraic effects (guest-defined handlers)

What makes me happiest long term

One mechanism. Today the host is a syscall table with suspension: perform always goes to perform_contextual, and all, fork, recording, replay, cancellation and dry-run each have their own path. End state: the host is only the outermost handler. Everything above it is a handler a Rust user can write, install, nest, and store by content hash. Recording is a handler. Replay is running under the recording handler. A dry-run that turns fs.write into a diff preview is a handler. Sandboxing is omission: a definition run with no exec handler in scope cannot exec, and loom-check can prove that at define time. Test doubles need no traits and no dependency injection: handle(fake_clock, || body()).

That is when "algebraic effects for stock Rust" is a plain statement of fact, and it is also when the REPL gets its best demo (agent runs a definition under the preview handler, FilesystemChanges.svelte shows what it would have done).

Existence check (run before starting)

rg -n "handle_push|Continuation|Forward|loom_handler_run" crates/   # expect 0 hits
rg -n "func_wrap_async" crates/loom-rt/src/sharedcore.rs             # 3 hits: perform, fork, join (the ABI you extend)

Fixed design decisions (no alternatives inside the lane)

1. Handler stack per execution scope. Guest.scope (sharedcore.rs) gains a stack of frames; a frame = (guest function-table index + data pointer, label set or *). Dispatch walks top-down. Forward continues down. Below the bottom is the existing host dispatcher, unchanged. Unhandled at the root errors exactly as today.
2. Deep handlers; handler body runs in the outer context. A handler stays installed for the rest of its body's dynamic extent. Performs made by a handler dispatch from the frame below its own, so a logging handler that performs fs.append cannot recurse into itself. This is the standard rule (Koka, OCaml 5); write it in the doc comment of handle.
3. Handler code runs on a fresh instance over the same shared memory, own 256 KiB stack + TLS, exactly how loom.fork runs loom_task_run today (sharedcore.rs:494-583). No new instance kind.
4. Inheritance: scope.fork children inherit the forker's stack at fork time (lexical: scope already joins children before the handle closure returns, so no frame outlives its children). loom::fork/loom::isolated::call (formerly loom::call) of another definition does not inherit: guest handlers live in one execution memory and never cross it. One sentence, one rule.
5. Continuations are one-shot values. Continuation<T>: !Clone, consumed by resume(v); abandon() is explicit; drop without resume aborts the suspended performer with EffectError("continuation dropped"). Never a silent leak, never a hang. Multi-shot is a non-goal (host fibers cannot clone a stack; OCaml 5 is one-shot and is still algebraic effects).
6. Recording sees only the root. A perform handled by a guest handler is guest computation, not a host effect; it is not recorded. Replay semantics unchanged. State this in docs/guide.md.
7. Pure contexts (fold): handle is allowed (it is pure); a perform reaching the root in pure code is still refused.
8. A trapping handler aborts the execution, same as a trapped scoped child today, error names the handler frame.
9. Cancellation unwinds the handler stack with the same drain order as scoped children (children, then frames, then borrowed parent storage).

ABI (core Wasm, same style as loom.perform/fork/join)

Imports in crates/loom-guest-rs/src/core.rs, host in sharedcore.rs::linker:

loom.handle_push(function: u32, data: u32, labels_ptr: u32, labels_len: u32) -> u64   // frame id, 0 = refused
loom.handle_pop(frame: u64) -> i32
loom.resume(k: u64, ptr: u32, len: u32) -> i32                                        // phase 2
loom.abandon(k: u64) -> i32                                                            // phase 2

Guest export, next to loom_task_run:

loom_handler_run(function: u32, data: u32, k: u64, op_ptr: u32, op_len: u32) -> u64

returning a packed DAG-CBOR response: {"resume": <value>} | {"forward": null} | {"deferred": null} (phase 2: handler stashed k, host parks the performer until loom.resume/loom.abandon). perform is unchanged for callers.

Guest API (loom-guest-rs)

pub fn handle<H, F, R>(handler: H, body: F) -> Result<R, EffectError>
where H: FnMut(Op, Continuation) -> Reply + Send, F: FnOnce() -> R;

pub struct Op { label: &str, args: Value }           // + op.arg::<T>()
pub enum Reply { Resume(Value), Forward, Deferred }  // Deferred only after phase 2
impl Continuation { pub fn resume<T: Serialize>(self, v: T) -> Result<(), EffectError>; pub fn abandon(self); }

Phase 1 ships Resume/Forward with Continuation present but only usable inline. Same Send + lexical bounds as scope.fork; the same admission scanner covers handler bodies (they are ordinary user code).

Phases, each with its goal command

The goal command is bun scripts/bench/effects-handlers.ts, printing N/13. Write it first, run it once for the 0/13 baseline, re-run after every landing.

Phase 0 (half a day): the script with all 13 gates stubbed to fail, the doc block above pasted into docs/plan-effects.md, ABI in docs/shared-core-abi.md.

Phase 1 (a few days): inline handlers. Gates 1-9:
1. fake clock: sleep under a handler returns instantly with the handler's value
2. forward: unhandled label reaches the host (now returns a real timestamp)
3. nesting: inner shadows outer; inner Forward reaches outer, not the host
4. handler body performs are dispatched to the outer context (logging handler performing fs.read of a fixture does not recurse; depth counter = 1)
5. scope.fork children inherit the handler
6. loom::fork of another definition does not inherit (its sleep is real)
7. trap inside a handler aborts the execution; error names the frame
8. handle inside a pure fold works; a root-bound perform in the fold is still refused
9. effect ledger: only root effects recorded (row count before/after under a fake-clock handler is unchanged)

After Phase 1 the tweet is honest as written.

Phase 2 (one to two weeks): continuations as values. Gates 10-12:
10. guest-implemented all (handler + run queue + deferred resume) returns the same result as host all on the same descriptors
11. drop-without-resume: error continuation dropped, no hang (gate has a 5 s timeout and fails on timeout)
12. abandon from a handler cancels the body; scoped children drained; memory limits still hold (shared-limits.ts stays 6/6)

Phase 3 (one week): effect rows checked at define time. Gate 13:
13. The rustc driver infers residual effect rows through concrete calls. A total handler removes its labels from the body row while keeping effects from its callback. Literal and Rust constant perform labels are accepted; runtime-selected labels fail checking with the call site named. The host enforces the inferred residual row.

Phase 4 (open-ended, the payoff): handlers as content-addressed definitions (handle_with(def_hash, body)), recording/replay reimplemented as the root handler, dry-run preview handler wired to FilesystemChanges.svelte, then delete the special cases in perform_contextual that the handler stack now covers. Each deletion greps the old path to zero.

Performance gate (measured, not asserted)

Add to the script: 10,000 performs handled by a trivial guest handler, report median and p99 round trip (guest perform → fresh instance → handler → resume). Target: median under 20 µs. Baseline with the pooling allocator will tell you whether a handler-instance cache is needed; do not build the cache before the number says so.

Non-goals (write into the plan so nobody re-derives them)

Multi-shot continuations. Rustc-level effect typing. In-guest handler jumps (the host round trip is the price of stock Rust; it is measured above). Backward compat for existing compiled guests: bump the ABI admission version, old artifacts rebuild.

Lane rules

Write-only lane: author code, tests, and the goal script; run no builds. One consolidated gate at the end. Work in a branch, not the shared checkout; another session is committing to this repo right now (5b495b1 landed at 01:24 PST while I read it).

## Baseline and implementation status

The requested existence checks found zero handler ABI symbols and three async
host imports before edits. `bun scripts/bench/effects-handlers.ts` ran with all
13 controls failing: **0/13**, first failure **fake clock**. Implementation is
isolated on `effects-guest-handlers`.

The consolidated native gate's fourth run passed **13/13** functional controls,
**6/6** phase-four controls, and **6/6** shared-memory limits, with exit 0.
This includes preserving the dropped-continuation diagnostic when guest code
calls `expect` on the returned error. Its immutable results are on
`dev-compute-4` under
`/home/andrew/loom-01a0866a-d0a0-7f00-96a7-5adb7517355a/effects-v4/gate.log`.

The third run's full 10,000-call benchmark measured **31.738 µs median,
52.709 µs p99**, above the **20 µs** median target. This measurement triggered
the bounded per-execution handler-instance cache experiment. The fourth run
measured **14.582 µs median, 24.406 µs p99** for the same compiled fixture and
verified 10,099 instance reuses including warmup. Reused instances reset TLS,
stack bounds, execution context, and handler references.

The sixth consolidated run passed the expanded **13/13** effects controls and
**6/6** phase-four controls, including cross-definition calls, fork-time handler
snapshots, nested pop, total-handler forwarding refusal, and active borrowed
handler cancellation. Cancellation observed active writes before stopping,
then verified zero surviving Stores and no writes after parent memory reuse.
The full benchmark measured **13.811 µs median, 24.356 µs p99**. Its results
are in the corresponding `effects-v6/gate.log` beside the fourth-run log.
Native test suites passed 218 tests, including 14 SDK tests and five compile-fail
controls; four diagnostic tests were ignored. The SDK's final Miri run passed
five controls. UI checking reported no errors or warnings, the production build
passed, and three compiled-component controls verified stale-response handling.
Visual inspection remains outstanding below.

## UI verification incident

Computer Use request through `mcp__node_repl__js`:
`var sky = (await import('@oai/sky')).sky; var apps = await sky.list_apps(); nodeRepl.write(JSON.stringify(apps));`
returned exactly `Sky Computer Use native pipe startup failed`.
This blocks desktop/browser visual inspection of the preview component. The
code artifact and non-UI integration gate continue; retry the Computer Use
connection or inspect from another session with a working connection. Static
UI checks are not a substitute for that visual verification.

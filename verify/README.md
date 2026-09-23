# Verified Loom programs: Rust to Lean, end to end

Two examples, one pipeline. Each is ordinary Rust with a pure core. Charon and
Aeneas translate the core into Lean, and Lean proves properties of the translated
code for every input, so the theorems are about the Rust that ships.

```sh
./check.sh   # charon -> aeneas -> lake build (proofs + checkers) -> axiom report
```

| Example | Rust | Lean | What is proved |
|---|---|---|---|
| Outbox delivery | `outbox/rust/src/lib.rs` | `lean/Outbox/` | No key is handled twice, and every row marked delivered was handled, under any sequence of pumps, acks and resets (a crash is a pump with no ack) |
| Tool-permission harness (a Loom actor) | `harness/harness.rs` | `lean/Harness/` | Five agent-harness properties (below), for every trace |

## The harness actor

`harness/harness.rs` is written as a Loom guest (a root-level `pub fn handle` plus
`LOOM_SCHEMA`, like `examples/unison/counter.rs`). It has two parts:

- `mod core`: a pure state machine, `step(&mut Harness, Event) -> Vec<Effect>`. Events
  are what the model, the user and the tool processes report (`ToolUse`, `Permission`,
  `ToolDone`, `Cancel`); effects are what the harness does (`AskPermission`, `RunTool`,
  `AbortTool`, `Result`).
- The shell (`handle`): loads the state from the actor's SQLite, calls `core::step`,
  writes the state and the emitted effects back. Loom runs each message in one
  transaction.

Charon translates only `crate::core` (`--start-from crate::core`) from the same source
file you give `loom add` (Loom may store a normalized reprint of it). The shell is the
trusted part, and its input domain is narrower than the proof's: ids must fit in i64
(SQLite INTEGER), and an unknown or multi-key message traps, so Loom dead-letters it
instead of acting on it.

### Proved for every trace (`lean/Harness/Proofs.lean`, carried to Rust by `Refinement.lean`)

| Property | Lean |
|---|---|
| P1 A tool runs only after the user was prompted for it and allowed it | `permission_first` |
| P2 The model never sees two results for one call | `at_most_one_result` |
| P3 A denial is final | `deny_final` |
| P4 After cancel nothing starts: no prompt, no tool run | `quiet_after_cancel` |
| P5 After cancel, every call has exactly one result | `closed_after_cancel` |

`Refinement.step_refines`: the generated Rust `core.step` returns `ok` and computes
`Spec.step`, given room for the pushes (`2 * calls + 2 < Usize.max`).
`Refinement.rust_harness_correct`: for any event list shorter than that bound, the
effects the Rust emits satisfy P1, P2, P3 and P5. `Refinement.rust_quiet_after_cancel`
is P4 for the Rust: for `pre ++ Cancel :: post`, the emitted effects split into a
before part and an after part, and the after part has no prompt and no tool run.

### The checker runs the real Rust

`lean/Harness/Check.lean` executes the Aeneas translation directly and enumerates every
interleaving of 9 events (two tool calls) to depth 5: 59,049 traces. It reports the
shortest counterexample per property. On the first draft, `core::step_v1` (kept in the
file), it finds:

- P4: `[toolUse 1, cancel, permission 1 true]` runs the tool after cancel.
- P2 and P5: `[toolUse 1, cancel, cancel]` sends two "cancelled" results. This one was not
  planted; it falls out of the same root cause (cancel reports calls without moving
  them to `Done`).

On `core::step` it finds nothing. Both verdicts are build gates, not printouts:
`step_passes_depth5` (all 59,049 traces clean) and `step_v1_fails_depth3` (the exact
shortest counterexamples above, a positive control so a checker that went blind fails
the build). A Rust step that panics or overflows is itself reported as a violation,
never dropped.

Negative control (2026-09-22): deleting `calls[i].phase = Phase::Done;` from the
`Asked` arm of `cancel_all` makes `check.sh` fail twice over: the checker reports P2 on
the real code, and `Refinement.lean` stops compiling. Restoring the line passes.

## The outbox

A model of the receipt path in `crates/loom-actor/src/pump.rs` (inject, then mark
delivered in a second transaction) and `crates/loom-actor/src/reset.rs` (a new
incarnation copies the `applied:` receipts). Loom's real message dedup is different:
`actor::inject` does `INSERT OR IGNORE INTO inbox` on a unique key, and `applied:`
receipts are written by control paths.

`Spec.exactly_once`: no key is handled twice, and every row marked delivered was
handled. `Spec.badReset_breaks_exactly_once`: a reset that drops receipts handles key 7
twice via `[pump 0, reset, pump 0]`. `Refinement.rust_exactly_once` is the same statement
for the Rust over whole traces from a fresh world, for traces short enough that no
counter overflows. Negative control: making the Rust `reset` clear `applied` fails
`reset_spec`.

Open question this surfaced (not a confirmed bug): `reset.rs` carries `applied:` receipts
and six tables forward but not `inbox`, so a message the sender redelivers after the
receiver reset (sender crashed before marking it delivered) is accepted again under the
new generation. If reset is meant to preserve at-most-once delivery across incarnations,
this breaks it; a test would settle it.

## What is trusted

- Charon and Aeneas translate Rust faithfully, and Aeneas's Lean models of the standard
  library (Vec, integer bounds) are right; Lean's kernel.
- Aeneas models checked arithmetic: overflow is an error. A release build without
  `overflow-checks` wraps instead, so the proofs assume overflow checks or the stated
  bounds.
- `native_decide` in `Check.lean` trusts Lean's compiler (those are tests, not proofs;
  no headline theorem depends on them, and `Axioms*.lean` enforces that).
- The shell code outside `core`, and Loom's one-message-one-transaction guarantee.
- P1 assumes `permission` messages come from the user. The guest API does not expose the
  sender (`crates/loom-guest-rs/src/actor.rs` has `send`, `accept`, `spawn`), so the shell
  cannot enforce it; any actor holding a send capability could approve a tool. Enforcing
  it needs a sender identity in the guest API or a separate capability for approvals.
- Nothing yet turns a `run_tool` row into a real process start; the proofs cover which
  effects are recorded.

## The gate

`check.sh` exits 0 only when the committed `Generated/` equals a fresh translation,
`lake build` passes (all proofs plus the checker gates), and `Axioms*.lean` match
exactly. `lake build` alone accepts `sorry` with rc=0; the axiom files catch it
(negative control 2026-09-22: a planted `sorry` in `rust_exactly_once` built with rc=0
and failed `AxiomsOutbox.lean` with rc=1).

## Tool versions (2026-09-22)

Aeneas and Charon from `github:AeneasVerif/aeneas` at
`12a018bb0fab3333be572dadc0eab5108758552b`
(`nix build --builders '' github:AeneasVerif/aeneas#aeneas github:AeneasVerif/aeneas#charon`);
Lean `v4.31.0`, Mathlib `v4.31.0`, pinned by that rev's `backends/lean/lakefile.lean`.

## Traps met while building this

- A bounded checker should test the observable property, not the proof invariant. The
  outbox checker first reported `[pump 0, reset]`, which breaks the invariant but has
  handled nothing twice.
- Aeneas's `Result` is an interaction tree, not an inductive with `ok`/`fail`
  constructors: evaluate with `(x).match` (see `Check.lean`).
- `step` discharges side conditions from hypotheses in context; state them as
  `v.length < Usize.max` (the `Vec.length` form) before the call.
- Two Aeneas outputs that both define an enum named `Event` collide on the generated
  `instDiscriminantEventIsize`; import them from separate files.

## References

- Aeneas: https://github.com/AeneasVerif/aeneas at `12a018bb` (loop reasoning:
  `backends/lean/Aeneas/Std/WP.lean` `loop.spec_decr_nat`; Vec specs:
  `backends/lean/Aeneas/Std/Vec.lean`; `Result.match`: `backends/lean/Aeneas/Std/Primitives.lean`)
- Charon: https://github.com/AeneasVerif/charon (`--start-from`)
- Lean 4: https://lean-lang.org

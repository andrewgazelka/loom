# Verified Loom programs: Rust to Lean, end to end

Two examples, one pipeline. Each is ordinary Rust with a pure core. Charon and
Aeneas translate the core into Lean, and Lean proves properties of the translated
code for every input, so the theorems are about the Rust that ships.

```sh
./check.sh   # charon -> aeneas -> lake build (proofs + checkers) -> axiom report
```

| Example | Rust | Lean | What is proved |
|---|---|---|---|
| Outbox delivery | `outbox/rust/src/lib.rs` | `lean/Outbox/` | Each key is handled exactly once, under any sequence of pumps, acks, crashes and resets |
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

Charon translates only `crate::core` (`--start-from crate::core`), from the exact file
Loom stores. The shell is the trusted part.

### Proved for every trace (`lean/Harness/Proofs.lean`, carried to Rust by `Refinement.lean`)

| Property | Lean |
|---|---|
| P1 A tool never runs without the user's permission | `permission_first` |
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

On `core::step` it finds nothing, and `step_passes_depth3` checks that verdict with
`native_decide`.

Negative control (2026-09-22): deleting `calls[i].phase = Phase::Done;` from the
`Asked` arm of `cancel_all` makes `check.sh` fail twice over: the checker reports P2 on
the real code, and `Refinement.lean` stops compiling. Restoring the line passes.

## The outbox

Mirrors `crates/loom-actor/src/pump.rs` (inject, then mark delivered in a second
transaction) and `crates/loom-actor/src/reset.rs` (a new incarnation copies the
`applied:` receipts). `Spec.exactly_once` proves no key is handled twice;
`Spec.badReset_breaks_exactly_once` shows a reset that drops receipts handles key 7
twice via `[pump 0, reset, pump 0]`; `Refinement.rust_step_exactly_once` carries it to
the Rust. Negative control: making the Rust `reset` clear `applied` fails `reset_spec`.

## What is trusted

- Charon and Aeneas translate Rust faithfully; Lean's kernel.
- `native_decide` in `Check.lean` trusts Lean's compiler (it is a test, not a proof;
  the proofs do not use it).
- The shell code outside `core`, and Loom's one-message-one-transaction guarantee.

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

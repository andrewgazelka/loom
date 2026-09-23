/-
Specification of the tool-permission core in `verify/harness/harness.rs`.

`step` mirrors `core::step` on Lean lists. The properties at the bottom are stated
over whole traces (every event list, every interleaving of model, user and tool),
and `Refinement.lean` carries them to the generated Rust.
-/
import Mathlib.Data.List.Nodup
import Mathlib.Data.List.Count

namespace Harness.Spec

inductive Phase where
  | asked | running | done
  deriving DecidableEq, Repr

inductive Outcome where
  | ok | denied | cancelled
  deriving DecidableEq, Repr

inductive Event where
  | toolUse (id : Nat)
  /-- `user` is true when the answer came from the user (Loom sender "external"). -/
  | permission (id : Nat) (allow : Bool) (user : Bool)
  | toolDone (id : Nat)
  | cancel
  deriving DecidableEq, Repr

inductive Effect where
  | ask (id : Nat)
  | run (id : Nat)
  | abort (id : Nat)
  | result (id : Nat) (o : Outcome)
  deriving DecidableEq, Repr

abbrev Calls := List (Nat × Phase)

structure H where
  calls : Calls
  cancelled : Bool
  deriving DecidableEq, Repr

def lookup : Calls → Nat → Option Phase
  | [], _ => none
  | c :: cs, id => if c.1 = id then some c.2 else lookup cs id

/-- Set the phase of the first call with this id, as `calls[find(id)].phase = p` does. -/
def upd : Calls → Nat → Phase → Calls
  | [], _, _ => []
  | c :: cs, id, p => if c.1 = id then (c.1, p) :: cs else c :: upd cs id p

def cancelOne : Nat × Phase → (Nat × Phase) × List Effect
  | (id, .asked) => ((id, .done), [.result id .cancelled])
  | (id, .running) => ((id, .done), [.abort id, .result id .cancelled])
  | (id, .done) => ((id, .done), [])

def step (h : H) : Event → H × List Effect
  | .toolUse id =>
    match lookup h.calls id with
    | none =>
      if h.cancelled then (⟨h.calls ++ [(id, .done)], true⟩, [.result id .cancelled])
      else (⟨h.calls ++ [(id, .asked)], false⟩, [.ask id])
    | some _ => (h, [])
  | .permission id allow user =>
    if user then
      match lookup h.calls id with
      | some .asked =>
        if allow then (⟨upd h.calls id .running, h.cancelled⟩, [.run id])
        else (⟨upd h.calls id .done, h.cancelled⟩, [.result id .denied])
      | _ => (h, [])
    else (h, [])
  | .toolDone id =>
    match lookup h.calls id with
    | some .running => (⟨upd h.calls id .done, h.cancelled⟩, [.result id .ok])
    | _ => (h, [])
  | .cancel => (⟨h.calls.map (cancelOne · |>.1), true⟩, h.calls.flatMap (cancelOne · |>.2))

/-- Run a trace; returns the final state and every effect emitted, in order. -/
def run (h : H) : List Event → H × List Effect
  | [] => (h, [])
  | e :: es =>
    let (h1, out1) := step h e
    let (h2, out2) := run h1 es
    (h2, out1 ++ out2)

def init : H := ⟨[], false⟩

/-! ## Trace properties (Bool, so the checker can evaluate them) -/

def isResultFor (id : Nat) : Effect → Bool
  | .result i _ => i == id
  | _ => false

def results (out : List Effect) (id : Nat) : Nat := out.countP (isResultFor id)

/-- P1: the tool never runs unless the user was prompted for it and the user (not the
model, not a tool) allowed it. -/
def PermissionFirst (es : List Event) (out : List Effect) : Prop :=
  ∀ id, .run id ∈ out → .ask id ∈ out ∧ .permission id true true ∈ es

/-- P2: the model never sees two results for one call. -/
def AtMostOneResult (out : List Effect) : Prop := ∀ id, results out id ≤ 1

/-- P3: a denial is final. -/
def DenyFinal (out : List Effect) : Prop :=
  ∀ id, .result id .denied ∈ out → .run id ∉ out

/-- P4: after cancel, nothing starts: no prompt, no tool run. -/
def QuietAfterCancel (h : H) (post : List Event) : Prop :=
  ∀ id, .run id ∉ (run h post).2 ∧ .ask id ∉ (run h post).2

/-- P5: once cancelled, every call the model made has exactly one result. -/
def ClosedAfterCancel (es : List Event) (out : List Effect) : Prop :=
  .cancel ∈ es → ∀ id, .toolUse id ∈ es → results out id = 1

end Harness.Spec

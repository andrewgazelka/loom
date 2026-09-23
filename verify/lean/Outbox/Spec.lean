/-
The specification: outbox delivery as a pure state machine over Lean lists.

This file is the reference model. It knows nothing about Rust; `Refinement.lean`
proves the Aeneas translation of `rust/src/lib.rs` computes exactly this.
-/
import Mathlib.Data.List.Nodup

namespace Outbox.Spec

structure Receiver where
  applied : List Nat
  handled : List Nat
  generation : Nat
  deriving DecidableEq, Repr

structure World where
  /-- Outbox row keys; payloads play no part in delivery. -/
  outbox : List Nat
  delivered : List Bool
  receiver : Receiver
  deriving DecidableEq, Repr

inductive Event where
  | pump (i : Nat)
  | ack (i : Nat)
  | reset
  deriving DecidableEq, Repr

def inject (r : Receiver) (k : Nat) : Receiver :=
  if k ∈ r.applied then r
  else { r with applied := r.applied ++ [k], handled := r.handled ++ [k] }

/-- `resetFn` is a parameter so the correct reset and the planted bug share one machine. -/
def step (resetFn : Receiver → Receiver) (w : World) : Event → World
  | .pump i =>
    match w.outbox[i]?, w.delivered[i]? with
    | some k, some false => { w with receiver := inject w.receiver k }
    | _, _ => w
  | .ack i =>
    match w.outbox[i]?, w.delivered[i]? with
    | some k, some _ =>
      if k ∈ w.receiver.applied then { w with delivered := w.delivered.set i true } else w
    | _, _ => w
  | .reset => { w with receiver := resetFn w.receiver }

def goodReset (r : Receiver) : Receiver := { r with generation := r.generation + 1 }

/-- The planted bug: the new incarnation forgets its receipts. -/
def badReset (r : Receiver) : Receiver := { r with applied := [], generation := r.generation + 1 }

def run (resetFn : Receiver → Receiver) (w : World) (es : List Event) : World :=
  es.foldl (step resetFn) w

def fresh (outbox : List Nat) : World :=
  { outbox, delivered := outbox.map fun _ => false, receiver := ⟨[], [], 0⟩ }

/-! ## Safety: every key is handled at most once -/

/-- The handled log equals the receipts, and the receipts have no duplicates. -/
def Inv (r : Receiver) : Prop := r.handled = r.applied ∧ r.applied.Nodup

instance (r : Receiver) : Decidable (Inv r) := by unfold Inv; infer_instance

theorem inject_inv {r : Receiver} (h : Inv r) (k : Nat) : Inv (inject r k) := by
  unfold inject
  split
  · exact h
  · next hk =>
    obtain ⟨he, hn⟩ := h
    refine ⟨by simp [he], ?_⟩
    simp only [List.nodup_append, List.nodup_singleton, List.mem_singleton, true_and]
    exact ⟨hn, fun a ha b hb hab => hk (hb ▸ hab ▸ ha)⟩

theorem step_inv {w : World} (h : Inv w.receiver) (e : Event) :
    Inv (step goodReset w e).receiver := by
  cases e with
  | pump i =>
    simp only [step]
    split
    · exact inject_inv h _
    · exact h
  | ack i =>
    simp only [step]
    split
    · split <;> exact h
    · exact h
  | reset => exact h

theorem run_inv {w : World} (h : Inv w.receiver) (es : List Event) :
    Inv (run goodReset w es).receiver := by
  induction es generalizing w with
  | nil => exact h
  | cons e es ih => exact ih (step_inv h e)

/-- The headline: from a fresh world, under ANY event sequence, no key is handled twice. -/
theorem exactly_once (outbox : List Nat) (es : List Event) :
    (run goodReset (fresh outbox) es).receiver.handled.Nodup := by
  have h := run_inv (w := fresh outbox) ⟨rfl, List.nodup_nil⟩ es
  exact h.1 ▸ h.2

/-! ## Progress: a pump followed by an ack delivers the row -/

theorem pump_ack_delivers (w : World) (i k : Nat)
    (ho : w.outbox[i]? = some k) (hd : w.delivered[i]? = some false) :
    (run goodReset w [.pump i, .ack i]).delivered[i]? = some true := by
  obtain ⟨hi, hdi⟩ := List.getElem?_eq_some_iff.mp hd
  have hk : k ∈ (inject w.receiver k).applied := by
    unfold inject; split <;> simp_all
  simp [run, step, ho, hdi, hk, hi]

/-! ## The planted bug is caught

Crash between inject and ack (so the row stays undelivered), reset, pump again. -/

def bugTrace : List Event := [.pump 0, .reset, .pump 0]

theorem badReset_handles_twice :
    (run badReset (fresh [7]) bugTrace).receiver.handled = [7, 7] := by decide

theorem badReset_breaks_exactly_once :
    ¬ ∀ outbox es, (run badReset (fresh outbox) es).receiver.handled.Nodup := by
  intro h
  have := h [7] bugTrace
  rw [badReset_handles_twice] at this
  simp at this

end Outbox.Spec

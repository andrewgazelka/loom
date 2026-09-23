/-
The bridge: the Aeneas translation of `rust/src/lib.rs` computes `Spec.step`.

`Generated/` is produced by Charon + Aeneas from the Rust source and never edited
by hand. Every theorem here is about that generated code, so `exactly_once`
carries over to the Rust that ships.
-/
import Aeneas
import Outbox.Generated.Funs
import Outbox.Spec

open Aeneas Aeneas.Std Result

namespace Outbox.Refinement

/-! ## Abstraction: Rust state to spec state -/

def absR (r : outbox_core.Receiver) : Spec.Receiver :=
  ⟨r.applied.val.map (·.val), r.handled.val.map (·.val), r.generation.val⟩

def absW (w : outbox_core.World) : Spec.World :=
  ⟨w.outbox.val.map (·.key.val), w.delivered.val, absR w.receiver⟩

def absE : outbox_core.Event → Spec.Event
  | .Pump i => .pump i.val
  | .Ack i => .ack i.val
  | .Reset => .reset

theorem mem_map_val {l : List U64} {k : U64} : k.val ∈ l.map (·.val) ↔ k ∈ l := by
  constructor
  · intro h
    obtain ⟨a, ha, he⟩ := List.mem_map.mp h
    have : a = k := by scalar_tac
    exact this ▸ ha
  · intro h; exact List.mem_map_of_mem h

/-! ## Function specs -/

theorem contains_loop_spec (keys : alloc.vec.Vec U64) (key : U64) (i : Usize)
    (hi : i.val ≤ keys.length) (hpre : key ∉ keys.val.take i.val) :
    outbox_core.contains_loop keys key i ⦃ b => b = decide (key ∈ keys.val) ⦄ := by
  unfold outbox_core.contains_loop
  apply loop.spec_decr_nat (fun j : Usize => keys.length - j.val)
    (fun j => j.val ≤ keys.length ∧ key ∉ keys.val.take j.val)
  · intro j ⟨hj, hnot⟩
    unfold outbox_core.contains_loop.body
    simp only [alloc.vec.Vec.index_slice_index]
    split
    · next hlt =>
      have hlt' : j.val < keys.length := by simpa [alloc.vec.Vec.len_val] using hlt
      step as ⟨x, hx⟩
      split
      · next heq =>
        simp only [WP.spec_ok]
        have : key ∈ keys.val := by
          rw [← heq, hx]; exact List.getElem_mem _
        simp [this]
      · next hne =>
        step as ⟨j1, hj1⟩
        refine ⟨by scalar_tac, ?_, by scalar_tac⟩
        rw [hj1, List.take_add_one]
        simp only [List.mem_append, not_or]
        refine ⟨hnot, ?_⟩
        simp [List.getElem?_eq_getElem hlt', hx] at *
        exact fun h => hne h.symm
    · next hge =>
      simp only [WP.spec_ok]
      have hge' : keys.length ≤ j.val := by
        simpa [alloc.vec.Vec.len_val] using hge
      rw [List.take_of_length_le hge'] at hnot
      simp [hnot]
  · exact ⟨hi, hpre⟩

@[step]
theorem contains_spec (keys : alloc.vec.Vec U64) (key : U64) :
    outbox_core.contains keys key ⦃ b => b = decide (key ∈ keys.val) ⦄ :=
  contains_loop_spec keys key 0#usize (by simp) (by simp)

/-- Pushing needs room below `Usize.max`; the spec's lists are unbounded. -/
def Room (r : outbox_core.Receiver) : Prop :=
  r.applied.length < Usize.max ∧ r.handled.length < Usize.max

theorem inject_spec (r : outbox_core.Receiver) (k : U64) (hroom : Room r) :
    outbox_core.inject r k ⦃ r' => absR r' = Spec.inject (absR r) k.val ⦄ := by
  unfold outbox_core.inject
  step as ⟨b, hb⟩
  split
  · next hT =>
    simp only [WP.spec_ok]
    have : k ∈ r.applied.val := by simpa [hT] using hb.symm
    simp [Spec.inject, absR, mem_map_val, this]
  · next hF =>
    have hk : k ∉ r.applied.val := by simpa [hF] using hb.symm
    obtain ⟨h1, h2⟩ := hroom
    step as ⟨v, hv⟩
    step as ⟨v1, hv1⟩
    simp [Spec.inject, absR, mem_map_val, hk, hv, hv1]

theorem reset_spec (r : outbox_core.Receiver) (h : r.generation.val < U64.max) :
    outbox_core.reset r ⦃ r' => absR r' = Spec.goodReset (absR r) ⦄ := by
  unfold outbox_core.reset
  step as ⟨g, hg⟩
  simp [absR, Spec.goodReset, hg]

/-- Room for one more receipt and one more reset: the Rust arithmetic cannot fail. -/
def Fits (w : outbox_core.World) : Prop :=
  Room w.receiver ∧ w.receiver.generation.val < U64.max

/-- The refinement theorem: one Rust step is one spec step. -/
theorem step_refines (w : outbox_core.World) (e : outbox_core.Event) (hfit : Fits w) :
    outbox_core.step w e ⦃ w' => absW w' = Spec.step Spec.goodReset (absW w) (absE e) ⦄ := by
  unfold outbox_core.step
  cases e with
  | Pump i =>
    simp only [absE, Spec.step]
    split
    · next hlo =>
      have hlo' : i.val < w.outbox.length := by simpa [alloc.vec.Vec.len_val] using hlo
      split
      · next hld =>
        have hld' : i.val < w.delivered.length := by simpa [alloc.vec.Vec.len_val] using hld
        simp only [alloc.vec.Vec.index_slice_index]
        step as ⟨b, hb⟩
        split
        · next hbT =>
          simp only [WP.spec_ok]
          simp [absW, List.getElem?_eq_getElem hlo', List.getElem?_eq_getElem hld', ← hb, hbT]
        · next hbF =>
          step as ⟨m, hm⟩
          have hroom := hfit.1
          step with inject_spec as ⟨r, hr⟩
          simp [absW, List.getElem?_eq_getElem hlo', List.getElem?_eq_getElem hld', ← hb, hbF,
            hr, hm]
      · next hld =>
        simp only [WP.spec_ok]
        have : ¬ i.val < w.delivered.length := by simpa [alloc.vec.Vec.len_val] using hld
        simp [absW, List.getElem?_eq_none (Nat.le_of_not_lt this)]
    · next hlo =>
      simp only [WP.spec_ok]
      have : ¬ i.val < w.outbox.length := by simpa [alloc.vec.Vec.len_val] using hlo
      simp [absW, List.getElem?_eq_none (Nat.le_of_not_lt this)]
  | Ack i =>
    simp only [absE, Spec.step]
    split
    · next hlo =>
      have hlo' : i.val < w.outbox.length := by simpa [alloc.vec.Vec.len_val] using hlo
      split
      · next hld =>
        have hld' : i.val < w.delivered.length := by simpa [alloc.vec.Vec.len_val] using hld
        simp only [alloc.vec.Vec.index_slice_index, alloc.vec.Vec.index_mut_slice_index]
        step as ⟨m, hm⟩
        step as ⟨b, hb⟩
        split
        · next hbT =>
          step as ⟨x, back, hx, hback⟩
          have hmem : m.key ∈ w.receiver.applied.val := by simpa [hbT] using hb.symm
          subst hm
          simp [absW, absR, List.getElem?_eq_getElem hlo', List.getElem?_eq_getElem hld',
            mem_map_val, hmem, hback, alloc.vec.Vec.set]
        · next hbF =>
          simp only [WP.spec_ok]
          have hmem : m.key ∉ w.receiver.applied.val := by simpa [hbF] using hb.symm
          subst hm
          simp [absW, absR, List.getElem?_eq_getElem hlo', List.getElem?_eq_getElem hld',
            mem_map_val, hmem]
      · next hld =>
        simp only [WP.spec_ok]
        have : ¬ i.val < w.delivered.length := by simpa [alloc.vec.Vec.len_val] using hld
        simp [absW, List.getElem?_eq_none (Nat.le_of_not_lt this)]
    · next hlo =>
      simp only [WP.spec_ok]
      have : ¬ i.val < w.outbox.length := by simpa [alloc.vec.Vec.len_val] using hlo
      simp [absW, List.getElem?_eq_none (Nat.le_of_not_lt this)]
  | Reset =>
    simp only [absE, Spec.step]
    have hgen := hfit.2
    step with reset_spec as ⟨r, hr⟩
    simp [absW, hr]

/-- What the Rust guarantees: every successful Rust step keeps the handled log free of
duplicates. Proved once in `Spec`, transported here by `step_refines`. -/
theorem rust_step_exactly_once (w : outbox_core.World) (e : outbox_core.Event)
    (hfit : Fits w) (hinv : Spec.Inv (absW w).receiver) :
    outbox_core.step w e ⦃ w' => ((absR w'.receiver).handled).Nodup ∧ Spec.Inv (absR w'.receiver) ⦄ := by
  have h := step_refines w e hfit
  apply WP.spec_mono h
  intro w' hw'
  have hi := Spec.step_inv hinv (absE e)
  rw [← hw'] at hi
  exact ⟨hi.1 ▸ hi.2, hi⟩

/-! ## Whole traces -/

/-- Run the generated Rust over a trace. -/
def rustRun (w : outbox_core.World) : List outbox_core.Event → Result outbox_core.World
  | [] => ok w
  | e :: es => do
    let w1 ← outbox_core.step w e
    rustRun w1 es

/-- Room for `n` more steps: each step pushes at most one receipt and one handled key,
and bumps the generation at most once. -/
def Bound (w : outbox_core.World) (n : Nat) : Prop :=
  w.receiver.applied.length + n < Usize.max ∧ w.receiver.handled.length + n < Usize.max ∧
    w.receiver.generation.val + n < U64.max

theorem spec_growth (w : Spec.World) (e : Spec.Event) :
    (Spec.step Spec.goodReset w e).receiver.applied.length ≤ w.receiver.applied.length + 1 ∧
    (Spec.step Spec.goodReset w e).receiver.handled.length ≤ w.receiver.handled.length + 1 ∧
    (Spec.step Spec.goodReset w e).receiver.generation ≤ w.receiver.generation + 1 := by
  have hi : ∀ r k, (Spec.inject r k).applied.length ≤ r.applied.length + 1 ∧
      (Spec.inject r k).handled.length ≤ r.handled.length + 1 ∧
      (Spec.inject r k).generation = r.generation := by
    intro r k; unfold Spec.inject; split <;> simp
  cases e with
  | pump i =>
    simp only [Spec.step]; split
    · have := hi w.receiver ‹_›; simp only; omega
    · omega
  | ack i => simp only [Spec.step]; split <;> (try split) <;> simp
  | reset => simp [Spec.step, Spec.goodReset]

theorem rustRun_refines (w : outbox_core.World) (es : List outbox_core.Event)
    (hb : Bound w es.length) :
    rustRun w es ⦃ w' => absW w' = Spec.run Spec.goodReset (absW w) (es.map absE) ⦄ := by
  induction es generalizing w with
  | nil => simp [rustRun, Spec.run]
  | cons e es ih =>
    obtain ⟨ha, hh, hg⟩ := hb
    simp only [List.length_cons, alloc.vec.Vec.length] at ha hh hg
    have hfit : Fits w := by
      unfold Fits Room; simp only [alloc.vec.Vec.length]
      refine ⟨⟨?_, ?_⟩, ?_⟩ <;> omega
    unfold rustRun
    step with step_refines as ⟨w1, hw1⟩
    have hg1 := spec_growth (absW w) (absE e)
    rw [← hw1] at hg1
    simp only [absW, absR, List.length_map] at hg1
    have hb1 : Bound w1 es.length := by
      unfold Bound; simp only [alloc.vec.Vec.length]
      refine ⟨?_, ?_, ?_⟩ <;> omega
    step with ih as ⟨w2, hw2⟩
    rw [hw2, hw1]
    rfl

/-- The headline for the Rust: from a fresh world, for any trace short enough that no
counter overflows, no key is handled twice and every delivered row was handled. -/
theorem rust_exactly_once (w : outbox_core.World) (keys : List Nat)
    (hw : absW w = Spec.fresh keys) (es : List outbox_core.Event) (hb : Bound w es.length) :
    rustRun w es ⦃ w' => (absW w').receiver.handled.Nodup ∧
      ∀ (i k : Nat), (absW w').outbox[i]? = some k → (absW w').delivered[i]? = some true →
        k ∈ (absW w').receiver.handled ⦄ := by
  apply WP.spec_mono (rustRun_refines w es hb)
  intro w' hw'
  rw [hw', hw]
  exact Spec.exactly_once keys (es.map absE)

end Outbox.Refinement

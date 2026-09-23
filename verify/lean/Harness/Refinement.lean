/-
The generated Rust (`Generated/`, from `verify/harness/harness.rs`) computes `Spec.step`,
so the five properties proved in `Proofs.lean` hold for the Rust itself.
-/
import Aeneas
import Harness.Generated.Funs
import Harness.Proofs

open Aeneas Aeneas.Std Result
open harness

namespace Harness.Refinement
open Spec

/-! ## Abstraction -/

def absPhase : core.Phase → Phase
  | .Asked => .asked | .Running => .running | .Done => .done

def absOutcome : core.Outcome → Outcome
  | .Ok => .ok | .Denied => .denied | .Cancelled => .cancelled

def absEff : core.Effect → Effect
  | .AskPermission id => .ask id.val
  | .RunTool id => .run id.val
  | .AbortTool id => .abort id.val
  | .Result id o => .result id.val (absOutcome o)

def absCalls (cs : List core.Call) : Calls := cs.map fun c => (c.id.val, absPhase c.phase)

def absH (h : core.Harness) : H := ⟨absCalls h.calls.val, h.cancelled⟩

def absE : core.Event → Event
  | .ToolUse id => .toolUse id.val
  | .Permission id a u => .permission id.val a u
  | .ToolDone id => .toolDone id.val
  | .Cancel => .cancel

/-! ## The first index with a given id -/

def fi (id : U64) : List core.Call → Nat
  | [] => 0
  | c :: cs => if c.id = id then 0 else fi id cs + 1

theorem fi_le (id : U64) (cs : List core.Call) : fi id cs ≤ cs.length := by
  induction cs with
  | nil => simp [fi]
  | cons c cs ih => simp only [fi]; split <;> simp; omega

theorem lookup_abs_none (id : U64) (cs : List core.Call) :
    fi id cs = cs.length ↔ lookup (absCalls cs) id.val = none := by
  induction cs with
  | nil => simp [fi, absCalls]
  | cons c cs ih =>
    simp only [fi, absCalls, List.map_cons, lookup, List.length_cons] at ih ⊢
    by_cases h : c.id = id
    · simp [h]
    · have : ¬ c.id.val = id.val := fun e => h (UScalar.val_eq_imp _ _ e)
      simp [h, this, ih]

theorem fi_get (id : U64) (cs : List core.Call) (hlt : fi id cs < cs.length) :
    cs[fi id cs].id = id ∧ lookup (absCalls cs) id.val = some (absPhase cs[fi id cs].phase) := by
  induction cs with
  | nil => simp [fi] at hlt
  | cons c cs ih =>
    by_cases h : c.id = id
    · simp [fi, h, absCalls, lookup]
    · have : ¬ c.id.val = id.val := fun e => h (UScalar.val_eq_imp _ _ e)
      have hlt' : fi id cs < cs.length := by simp [fi, h] at hlt; omega
      obtain ⟨h1, h2⟩ := ih hlt'
      simp only [fi, h, if_false, List.getElem_cons_succ]
      exact ⟨h1, by simpa [absCalls, lookup, this] using h2⟩

theorem fi_set (id : U64) (cs : List core.Call) (hlt : fi id cs < cs.length) (p : core.Phase) :
    absCalls (cs.set (fi id cs) { cs[fi id cs] with phase := p }) =
      upd (absCalls cs) id.val (absPhase p) := by
  induction cs with
  | nil => simp [fi] at hlt
  | cons c cs ih =>
    by_cases h : c.id = id
    · simp [fi, h, absCalls, upd]
    · have : ¬ c.id.val = id.val := fun e => h (UScalar.val_eq_imp _ _ e)
      have hlt' : fi id cs < cs.length := by simp [fi, h] at hlt; omega
      have := ih hlt'
      simp only [fi, h, if_false, List.getElem_cons_succ, List.set_cons_succ]
      simp_all [absCalls, upd]

theorem fi_char (id : U64) (cs : List core.Call) (j : Nat) (hj : j ≤ cs.length)
    (hbefore : ∀ k (hk : k < j), (cs[k]'(by omega)).id ≠ id) :
    (∀ hjl : j < cs.length, cs[j].id = id → fi id cs = j) ∧ (j = cs.length → fi id cs = j) := by
  induction cs generalizing j with
  | nil => simp at hj; subst hj; simp [fi]
  | cons c cs ih =>
    cases j with
    | zero =>
      refine ⟨fun _ h => by simp at h; simp [fi, h], fun h => by simp at h⟩
    | succ j =>
      have hc : c.id ≠ id := hbefore 0 (by omega)
      have := ih j (by simp at hj; omega) (fun k hk => by simpa using hbefore (k + 1) (by omega))
      refine ⟨fun hjl h => ?_, fun h => ?_⟩
      · simp only [fi, hc, if_false]; simp at hjl; rw [this.1 hjl (by simpa using h)]
      · simp only [fi, hc, if_false]; simp at h; rw [this.2 h]

/-! ## `find` -/

@[step]
theorem find_spec (calls : alloc.vec.Vec core.Call) (id : U64) :
    core.find calls id ⦃ i => i.val = fi id calls.val ⦄ := by
  unfold core.find core.find_loop
  apply loop.spec_decr_nat (fun j : Usize => calls.length - j.val)
    (fun j => j.val ≤ calls.length ∧ ∀ k (hk : k < j.val), ∀ hk' : k < calls.val.length,
      (calls.val[k]'hk').id ≠ id)
  · intro j ⟨hj, hbefore⟩
    have hc := fi_char id calls.val j.val hj (fun k hk => hbefore k hk _)
    unfold core.find_loop.body
    simp only [alloc.vec.Vec.index_slice_index]
    split
    · next hlt =>
      have hlt' : j.val < calls.length := by simpa [alloc.vec.Vec.len_val] using hlt
      step as ⟨c, hcv⟩
      split
      · next heq => simp only [WP.spec_ok]; exact (hc.1 hlt' (hcv ▸ heq)).symm
      · next hne =>
        step as ⟨j1, hj1⟩
        refine ⟨by scalar_tac, fun k hk hk' => ?_, by scalar_tac⟩
        by_cases hkj : k = j.val
        · subst hkj; rw [← hcv]; exact hne
        · exact hbefore k (by omega) hk'
    · next hge =>
      simp only [WP.spec_ok]
      have : j.val = calls.length := by
        have : ¬ j.val < calls.length := by simpa [alloc.vec.Vec.len_val] using hge
        omega
      simp [alloc.vec.Vec.len_val, (hc.2 this), this]
  · exact ⟨by simp, fun k hk => by simp at hk⟩

/-! ## `cancel_all` -/

def cancelR (c : core.Call) : core.Call := { c with phase := .Done }

def effR (c : core.Call) : List core.Effect :=
  match c.phase with
  | .Asked => [.Result c.id .Cancelled]
  | .Running => [.AbortTool c.id, .Result c.id .Cancelled]
  | .Done => []

theorem effR_length (c : core.Call) : (effR c).length ≤ 2 := by
  unfold effR; split <;> simp

theorem flatMap_effR_length (cs : List core.Call) : (cs.flatMap effR).length ≤ 2 * cs.length := by
  induction cs with
  | nil => simp
  | cons c cs ih => simp only [List.flatMap_cons, List.length_append, List.length_cons]; have := effR_length c; omega

theorem cancel_all_spec (calls : alloc.vec.Vec core.Call) (out : alloc.vec.Vec core.Effect)
    (hroom : out.length + 2 * calls.length < Usize.max) :
    core.cancel_all calls out ⦃ calls' out' =>
      calls'.val = calls.val.map cancelR ∧ out'.val = out.val ++ calls.val.flatMap effR ⦄ := by
  unfold core.cancel_all core.cancel_all_loop
  apply loop.spec_decr_nat (fun (x : alloc.vec.Vec core.Call × alloc.vec.Vec core.Effect × Usize) =>
      calls.length - x.2.2.val)
    (fun x => x.2.2.val ≤ calls.length ∧
      x.1.val = (calls.val.take x.2.2.val).map cancelR ++ calls.val.drop x.2.2.val ∧
      x.2.1.val = out.val ++ (calls.val.take x.2.2.val).flatMap effR)
    (fun (r : alloc.vec.Vec core.Call × alloc.vec.Vec core.Effect) =>
      r.1.val = calls.val.map cancelR ∧ r.2.val = out.val ++ calls.val.flatMap effR)
  · rintro ⟨cs, o, j⟩ ⟨hj, hcs, ho⟩
    simp only at hj hcs ho
    simp only [alloc.vec.Vec.length] at hj hroom
    have hpre : ((calls.val.take j.val).map cancelR).length = j.val := by simp; omega
    have hlen : cs.val.length = calls.val.length := by rw [hcs]; simp; omega
    have hoLen : o.val.length ≤ out.val.length + 2 * j.val := by
      rw [ho]; have := flatMap_effR_length (calls.val.take j.val); simp at this ⊢; omega
    unfold core.cancel_all_loop.body
    simp only [alloc.vec.Vec.index_slice_index, alloc.vec.Vec.index_mut_slice_index]
    split
    · next hlt =>
      have hlt' : j.val < calls.val.length := by
        have : j.val < cs.val.length := by simpa [alloc.vec.Vec.len_val] using hlt
        omega
      have hget : cs.val[j.val]'(by omega) = calls.val[j.val] := by
        have h1 : cs.val[j.val]? = some calls.val[j.val] := by
          rw [hcs, List.getElem?_append_right (by rw [hpre]), hpre]; simp
        rw [List.getElem?_eq_getElem (by omega)] at h1
        exact Option.some.inj h1
      have htake : calls.val.take (j.val + 1) = calls.val.take j.val ++ [calls.val[j.val]] :=
        List.take_succ_eq_append_getElem hlt'
      have hdrop : calls.val.drop j.val = calls.val[j.val] :: calls.val.drop (j.val + 1) :=
        List.drop_eq_getElem_cons hlt'
      have hset : ∀ x : core.Call, cs.val.set j.val x =
          (calls.val.take j.val).map cancelR ++ x :: calls.val.drop (j.val + 1) := by
        intro x
        rw [hcs, List.set_append_right _ _ (by rw [hpre]), hpre, Nat.sub_self,
          List.drop_eq_getElem_cons hlt']
        rfl
      step as ⟨c, hc⟩
      have hc' : c = calls.val[j.val] := hc.trans hget
      subst hc'
      rcases hph : calls.val[j.val].phase with _ | _ | _
      · simp only [hph]
        step as ⟨p, hp1, hp2⟩
        obtain ⟨c1, back⟩ := p
        simp only at hp1 hp2 ⊢
        have : o.length < Usize.max := by simp only [alloc.vec.Vec.length]; omega
        step as ⟨o1, ho1⟩
        step as ⟨j1, hj1⟩
        simp only [alloc.vec.Vec.length]
        refine ⟨by omega, ?_, ?_, by omega⟩
        · rw [hp2, hp1, hget]
          simp only [alloc.vec.Vec.set]
          rw [hj1, htake]
          simp [hset, cancelR]
          rw [List.take_succ_eq_append_getElem (by simp; omega), List.append_assoc, List.getElem_map]
          rfl
        · rw [ho1, ho, hj1, htake]; simp only [List.flatMap_append, List.flatMap_cons, List.flatMap_nil, List.append_nil, effR, hph, List.append_assoc]
      · simp only [hph]
        step as ⟨p, hp1, hp2⟩
        obtain ⟨c1, back⟩ := p
        simp only at hp1 hp2 ⊢
        have : o.length < Usize.max := by simp only [alloc.vec.Vec.length]; omega
        step as ⟨o1, ho1⟩
        have : o1.length < Usize.max := by simp only [alloc.vec.Vec.length]; rw [ho1]; simp; omega
        step as ⟨o2, ho2⟩
        step as ⟨j1, hj1⟩
        simp only [alloc.vec.Vec.length]
        refine ⟨by omega, ?_, ?_, by omega⟩
        · rw [hp2, hp1, hget]
          simp only [alloc.vec.Vec.set]
          rw [hj1, htake]
          simp [hset, cancelR]
          rw [List.take_succ_eq_append_getElem (by simp; omega), List.append_assoc, List.getElem_map]
          rfl
        · rw [ho2, ho1, ho, hj1, htake]; simp only [List.flatMap_append, List.flatMap_cons, List.flatMap_nil, List.append_nil, effR, hph, List.append_assoc]; rfl
      · simp only [hph]
        step as ⟨j1, hj1⟩
        simp only [alloc.vec.Vec.length]
        refine ⟨by omega, ?_, ?_, by omega⟩
        · rw [hcs, hj1, htake, hdrop]
          have : cancelR calls.val[j.val] = calls.val[j.val] := by
            unfold cancelR; rw [← hph]
          rw [List.map_append, List.append_assoc, List.map_singleton, this]; rfl
        · rw [ho, hj1, htake]; simp only [List.flatMap_append, List.flatMap_cons, List.flatMap_nil, List.append_nil, effR, hph, List.append_assoc]
    · next hge =>
      simp only [WP.spec_ok]
      have : j.val = calls.val.length := by
        have : ¬ j.val < cs.val.length := by simpa [alloc.vec.Vec.len_val] using hge
        omega
      rw [this] at hcs ho
      simp [hcs, ho]
  · simp

theorem absCalls_cancel (cs : List core.Call) :
    absCalls (cs.map cancelR) = (absCalls cs).map (cancelOne · |>.1) := by
  simp [absCalls, cancelR, absPhase, cancelOne_fst]

theorem absEff_cancel (cs : List core.Call) :
    (cs.flatMap effR).map absEff = (absCalls cs).flatMap (cancelOne · |>.2) := by
  induction cs with
  | nil => rfl
  | cons c cs ih =>
    simp only [List.flatMap_cons, List.map_append, ih, absCalls, List.map_cons]
    congr 1
    obtain ⟨id, p⟩ := c
    cases p <;> simp [effR, cancelOne, absEff, absOutcome, absPhase]

/-! ## `step` -/

/-- Enough headroom that no push in `step` can overflow. -/
def Fits (h : core.Harness) : Prop := 2 * h.calls.length + 2 < Usize.max

theorem step_refines (h : core.Harness) (e : core.Event) (hfit : Fits h) :
    core.step h e ⦃ out h' =>
      absH h' = (step (absH h) (absE e)).1 ∧ out.val.map absEff = (step (absH h) (absE e)).2 ⦄ := by
  unfold core.step
  unfold Fits at hfit
  cases e with
  | ToolUse id =>
    step as ⟨i, hi⟩
    simp only [absE, step, absH]
    by_cases hnone : fi id h.calls.val = h.calls.val.length
    · have hl := (lookup_abs_none id h.calls.val).mp hnone
      have hieq : i = alloc.vec.Vec.len h.calls := by
        apply UScalar.val_eq_imp; simp [alloc.vec.Vec.len_val, hi, hnone]
      simp only [hieq, if_true, hl]
      by_cases hc : h.cancelled
      · simp only [hc, if_true]
        step as ⟨v, hv⟩
        step as ⟨o, ho⟩
        simp [hv, ho, absCalls, absPhase, absEff, absOutcome]
      · simp only [hc, Bool.false_eq_true, if_false]
        step as ⟨v, hv⟩
        step as ⟨o, ho⟩
        simp [hv, ho, absCalls, absPhase, absEff]
    · have hlt : fi id h.calls.val < h.calls.val.length := by
        have := fi_le id h.calls.val; omega
      have hne : ¬ i = alloc.vec.Vec.len h.calls := by
        intro heq; apply hnone
        have := congrArg UScalar.val heq; simp [alloc.vec.Vec.len_val] at this; omega
      obtain ⟨-, hl⟩ := fi_get id h.calls.val hlt
      simp [hne, hl]
  | Permission id allow u =>
    step as ⟨i, hi⟩
    cases u
    · simp [absE, step, absH]
    simp only [absE, step, absH, if_true]
    by_cases hlt : fi id h.calls.val < h.calls.val.length
    · obtain ⟨hid, hl⟩ := fi_get id h.calls.val hlt
      have hlt' : i < alloc.vec.Vec.len h.calls := by
        show i.val < (alloc.vec.Vec.len h.calls).val; simp [alloc.vec.Vec.len_val, hi, hlt]
      simp only [hlt', if_true, alloc.vec.Vec.index_slice_index,
        alloc.vec.Vec.index_mut_slice_index, hl]
      step as ⟨c, hc⟩
      have hc' : c = h.calls.val[fi id h.calls.val] := by simp [hc, hi]
      rw [hc']
      rcases hph : h.calls.val[fi id h.calls.val].phase with _ | _ | _
      · simp only [absPhase]
        split
        · next ha =>
          subst ha
          step as ⟨c1, back, hc1, hback⟩
          step as ⟨o, ho⟩
          refine ⟨?_, by simp [ho, absEff]⟩
          simp only [hback, hc1, alloc.vec.Vec.set, hi]
          have := fi_set id h.calls.val hlt .Running
          simp_all [absPhase]
        · next ha =>
          simp only [Bool.not_eq_true] at ha; subst ha
          step as ⟨c1, back, hc1, hback⟩
          step as ⟨o, ho⟩
          refine ⟨?_, by simp [ho, absEff, absOutcome]⟩
          simp only [hback, hc1, alloc.vec.Vec.set, hi]
          have := fi_set id h.calls.val hlt .Done
          simp_all [absPhase]
      · simp [absPhase]
      · simp [absPhase]
    · have hnone : fi id h.calls.val = h.calls.val.length := by have := fi_le id h.calls.val; omega
      have hl := (lookup_abs_none id h.calls.val).mp hnone
      have hge : ¬ i < alloc.vec.Vec.len h.calls := by
        show ¬ i.val < (alloc.vec.Vec.len h.calls).val; simp [alloc.vec.Vec.len_val, hi, hnone]
      simp [hge, hl]
  | ToolDone id =>
    step as ⟨i, hi⟩
    simp only [absE, step, absH]
    by_cases hlt : fi id h.calls.val < h.calls.val.length
    · obtain ⟨hid, hl⟩ := fi_get id h.calls.val hlt
      have hlt' : i < alloc.vec.Vec.len h.calls := by
        show i.val < (alloc.vec.Vec.len h.calls).val; simp [alloc.vec.Vec.len_val, hi, hlt]
      simp only [hlt', if_true, alloc.vec.Vec.index_slice_index,
        alloc.vec.Vec.index_mut_slice_index, hl]
      step as ⟨c, hc⟩
      have hc' : c = h.calls.val[fi id h.calls.val] := by simp [hc, hi]
      rw [hc']
      rcases hph : h.calls.val[fi id h.calls.val].phase with _ | _ | _
      · simp [absPhase]
      · simp only [absPhase]
        step as ⟨c1, back, hc1, hback⟩
        step as ⟨o, ho⟩
        refine ⟨?_, by simp [ho, absEff, absOutcome]⟩
        simp only [hback, hc1, alloc.vec.Vec.set, hi]
        have := fi_set id h.calls.val hlt .Done
        simp_all [absPhase]
      · simp [absPhase]
    · have hnone : fi id h.calls.val = h.calls.val.length := by have := fi_le id h.calls.val; omega
      have hl := (lookup_abs_none id h.calls.val).mp hnone
      have hge : ¬ i < alloc.vec.Vec.len h.calls := by
        show ¬ i.val < (alloc.vec.Vec.len h.calls).val; simp [alloc.vec.Vec.len_val, hi, hnone]
      simp [hge, hl]
  | Cancel =>
    simp only [absE, step, absH]
    step with cancel_all_spec as ⟨v, o, hv, ho⟩
    simp [hv, ho, absCalls_cancel, absEff_cancel]

/-! ## Whole traces: the Rust satisfies all five properties -/

/-- Run the generated Rust over a trace, collecting every effect. -/
def rustRun (h : core.Harness) : List core.Event → Result (List core.Effect × core.Harness)
  | [] => ok ([], h)
  | e :: es => do
    let (out, h1) ← core.step h e
    let (rest, h2) ← rustRun h1 es
    ok (out.val ++ rest, h2)

theorem step_calls_length (h : H) (e : Event) :
    (step h e).1.calls.length ≤ h.calls.length + 1 := by
  have hu : ∀ cs id p, (upd cs id p).length = cs.length := by
    intro cs id p; induction cs with
    | nil => rfl
    | cons c cs ih => simp only [upd]; split <;> simp [ih]
  cases e <;> simp only [step] <;> (try split) <;> (try split) <;> (try split) <;> simp [hu]

theorem rustRun_refines (h : core.Harness) (es : List core.Event)
    (hfit : 2 * (h.calls.length + es.length) + 2 < Usize.max) :
    rustRun h es ⦃ out h' =>
      out.map absEff = (run (absH h) (es.map absE)).2 ∧
      absH h' = (run (absH h) (es.map absE)).1 ⦄ := by
  induction es generalizing h with
  | nil => simp [rustRun, run]
  | cons e es ih =>
    unfold rustRun
    step with step_refines as ⟨out, h1, hh1, hout⟩
    · unfold Fits; simp [alloc.vec.Vec.length] at hfit ⊢; omega
    have hlen : h1.calls.length ≤ h.calls.length + 1 := by
      have := step_calls_length (absH h) (absE e)
      rw [← hh1] at this
      simpa [absH, absCalls, alloc.vec.Vec.length] using this
    step with ih as ⟨rest, h2, hrest, hh2⟩
    simp only [WP.spec_ok, List.map_append, List.map_cons, run]
    rw [hout, hrest, hh2, hh1]
    exact ⟨rfl, rfl⟩

def rustInit : core.Harness := ⟨alloc.vec.Vec.new _, false⟩

theorem absH_init : absH rustInit = Spec.init := rfl

/-- The headline: for any trace the Rust accepts without overflow, all five
properties hold of the effects it actually emits. -/
theorem rust_harness_correct (es : List core.Event) (hfit : 2 * es.length + 2 < Usize.max) :
    rustRun rustInit es ⦃ out _ =>
      PermissionFirst (es.map absE) (out.map absEff) ∧
      AtMostOneResult (out.map absEff) ∧
      DenyFinal (out.map absEff) ∧
      ClosedAfterCancel (es.map absE) (out.map absEff) ⦄ := by
  have h := rustRun_refines rustInit es (by simpa [rustInit] using hfit)
  apply WP.spec_mono h
  rintro ⟨out, h'⟩ ⟨hout, -⟩
  simp only [WP.uncurry'_pair] at *
  rw [hout, absH_init]
  exact ⟨permission_first _, at_most_one_result _, deny_final _, closed_after_cancel _⟩

/-- P4 for the Rust: split the emitted effects at the cancel; the part after it
contains no prompt and no tool run. -/
theorem rust_quiet_after_cancel (pre post : List core.Event)
    (hfit : 2 * (pre.length + post.length + 1) + 2 < Usize.max) :
    rustRun rustInit (pre ++ core.Event.Cancel :: post) ⦃ out _ =>
      ∃ before after, out.map absEff = before ++ after ∧
        ∀ id, .run id ∉ after ∧ .ask id ∉ after ⦄ := by
  have h := rustRun_refines rustInit (pre ++ core.Event.Cancel :: post)
    (by simp [rustInit]; omega)
  apply WP.spec_mono h
  rintro ⟨out, h'⟩ ⟨hout, -⟩
  simp only [WP.uncurry'_pair] at *
  rw [hout, absH_init]
  have hsplit : (pre ++ core.Event.Cancel :: post).map absE =
      (pre.map absE ++ [.cancel]) ++ post.map absE := by simp [absE]
  rw [hsplit, run_append]
  exact ⟨_, _, rfl, quiet_after_cancel (pre.map absE) (post.map absE)⟩

end Harness.Refinement

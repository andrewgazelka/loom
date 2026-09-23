/-
Proofs of the five harness properties for EVERY trace, of any length.
The checker in `Check.lean` covers depth 5 on the real code; this covers all depths
on the spec, and `Refinement.lean` connects the two.
-/
import Harness.Spec
import Mathlib.Data.List.Induction

namespace Harness.Spec

/-! ## Lemmas about the call list -/

@[simp] theorem lookup_nil (id : Nat) : lookup [] id = none := rfl

theorem lookup_append (cs ds : Calls) (id : Nat) :
    lookup (cs ++ ds) id = (lookup cs id).or (lookup ds id) := by
  induction cs with
  | nil => simp
  | cons c cs ih => simp only [List.cons_append, lookup]; split <;> simp_all

theorem lookup_single (i id : Nat) (p : Phase) :
    lookup [(i, p)] id = if i = id then some p else none := by
  simp [lookup]

theorem lookup_eq_none_iff (cs : Calls) (id : Nat) :
    lookup cs id = none ↔ id ∉ cs.map (·.1) := by
  induction cs with
  | nil => simp
  | cons c cs ih =>
    simp only [lookup, List.map_cons, List.mem_cons, not_or]
    split <;> simp_all [eq_comm]

theorem lookup_upd (cs : Calls) (id id' : Nat) (p : Phase) :
    lookup (upd cs id p) id' =
      if id' = id ∧ (lookup cs id).isSome then some p else lookup cs id' := by
  induction cs with
  | nil => simp [upd]
  | cons c cs ih =>
    by_cases h1 : c.1 = id
    · by_cases h2 : id' = id
      · subst h2; simp [upd, lookup, h1]
      · have h4 : ¬ id = id' := fun h => h2 h.symm
        simp [upd, lookup, h1, h4, h2]
    · by_cases h3 : c.1 = id'
      · have h2 : ¬ id' = id := fun h => h1 (h3 ▸ h)
        simp [upd, lookup, h1, h3, h2]
      · simp [upd, lookup, h1, h3, ih]

theorem keys_upd (cs : Calls) (id : Nat) (p : Phase) :
    (upd cs id p).map (·.1) = cs.map (·.1) := by
  induction cs with
  | nil => rfl
  | cons c cs ih => simp only [upd]; split <;> simp_all

theorem cancelOne_fst (c : Nat × Phase) : (cancelOne c).1 = (c.1, .done) := by
  obtain ⟨i, p⟩ := c; cases p <;> rfl

theorem lookup_cancel (cs : Calls) (id : Nat) :
    lookup (cs.map (cancelOne · |>.1)) id = (lookup cs id).map (fun _ => .done) := by
  induction cs with
  | nil => rfl
  | cons c cs ih =>
    simp only [List.map_cons, cancelOne_fst] at ih ⊢
    by_cases hc : c.1 = id <;> simp [lookup, hc, ih]

theorem keys_cancel (cs : Calls) : (cs.map (cancelOne · |>.1)).map (·.1) = cs.map (·.1) := by
  induction cs with
  | nil => rfl
  | cons c cs ih =>
    simp only [List.map_cons, cancelOne_fst] at ih ⊢
    rw [ih]

theorem all_done_lookup {cs : Calls} (h : ∀ c ∈ cs, c.2 = .done) (id : Nat) :
    lookup cs id = none ∨ lookup cs id = some .done := by
  induction cs with
  | nil => simp
  | cons c cs ih =>
    simp only [lookup]
    split
    · exact .inr (by rw [h c (by simp)])
    · exact ih fun d hd => h d (by simp [hd])

/-! ## Lemmas about effects -/

theorem cancelOne_no_run (c : Nat × Phase) (id : Nat) :
    .run id ∉ (cancelOne c).2 ∧ .ask id ∉ (cancelOne c).2 ∧ .result id .denied ∉ (cancelOne c).2 := by
  obtain ⟨i, p⟩ := c; cases p <;> simp [cancelOne]

theorem cancel_no_run (cs : Calls) (id : Nat) :
    .run id ∉ cs.flatMap (cancelOne · |>.2) ∧ .ask id ∉ cs.flatMap (cancelOne · |>.2) ∧
      .result id .denied ∉ cs.flatMap (cancelOne · |>.2) := by
  simp only [List.mem_flatMap, not_exists, not_and]
  exact ⟨fun c _ => (cancelOne_no_run c id).1, fun c _ => (cancelOne_no_run c id).2.1,
    fun c _ => (cancelOne_no_run c id).2.2⟩

theorem results_append (a b : List Effect) (id : Nat) :
    results (a ++ b) id = results a id + results b id := by
  simp [results, List.countP_append]

theorem results_cancelOne (c : Nat × Phase) (id : Nat) :
    results (cancelOne c).2 id = if c.1 = id ∧ c.2 ≠ .done then 1 else 0 := by
  obtain ⟨i, p⟩ := c; cases p <;> by_cases h : i = id <;> simp [cancelOne, results, isResultFor, h]

theorem results_cancel (cs : Calls) (hn : (cs.map (·.1)).Nodup) (id : Nat) :
    results (cs.flatMap (cancelOne · |>.2)) id =
      if lookup cs id = some .asked ∨ lookup cs id = some .running then 1 else 0 := by
  induction cs with
  | nil => simp [results]
  | cons c cs ih =>
    simp only [List.map_cons, List.nodup_cons] at hn
    simp only [List.flatMap_cons, results_append, results_cancelOne, ih hn.2, lookup]
    by_cases hc : c.1 = id
    · have : lookup cs id = none := (lookup_eq_none_iff cs id).mpr (hc ▸ hn.1)
      obtain ⟨i, p⟩ := c
      cases p <;> simp_all
    · simp [hc]

@[simp] theorem results_nil (id : Nat) : results [] id = 0 := rfl

@[simp] theorem results_result (i id : Nat) (o : Outcome) :
    results [.result i o] id = if i = id then 1 else 0 := by
  by_cases h : i = id <;> simp [results, isResultFor, h]

@[simp] theorem results_ask (i id : Nat) : results [.ask i] id = 0 := rfl
@[simp] theorem results_run (i id : Nat) : results [.run i] id = 0 := rfl

theorem nodup_snoc {cs : Calls} {id : Nat} {p : Phase} (hn : (cs.map (·.1)).Nodup)
    (hl : lookup cs id = none) : ((cs ++ [(id, p)]).map (·.1)).Nodup := by
  have hnot := (lookup_eq_none_iff cs id).mp hl
  rw [List.map_append]
  refine List.nodup_append.mpr ⟨hn, by simp, fun a ha b hb hab => hnot ?_⟩
  simp only [List.map_cons, List.map_nil, List.mem_singleton] at hb
  exact hb ▸ hab ▸ ha

/-! ## The invariant -/

structure Inv (es : List Event) (h : H) (out : List Effect) : Prop where
  nodup : (h.calls.map (·.1)).Nodup
  count : ∀ id, results out id = if lookup h.calls id = some .done then 1 else 0
  perm : ∀ id, .run id ∈ out → .permission id true ∈ es
  closed : h.cancelled = true → ∀ c ∈ h.calls, c.2 = .done
  flag : h.cancelled = true ↔ .cancel ∈ es
  known : ∀ id, (lookup h.calls id).isSome ↔ .toolUse id ∈ es
  fresh : ∀ id, lookup h.calls id = none → .run id ∉ out
  asked : ∀ id, lookup h.calls id = some .asked → .run id ∉ out
  deny : ∀ id, .run id ∈ out → .result id .denied ∉ out
  askOpen : ∀ id, (lookup h.calls id = some .asked ∨ lookup h.calls id = some .running) →
    .ask id ∈ out
  runAsk : ∀ id, .run id ∈ out → .ask id ∈ out

theorem inv_init : Inv [] init [] where
  nodup := by simp [init]
  count := by simp [init, results]
  perm := by simp
  closed := by simp [init]
  flag := by simp [init]
  known := by simp [init]
  fresh := by simp
  asked := by simp
  deny := by simp
  askOpen := by simp [init]
  runAsk := by simp

theorem results_zero_of_not_done {es h out} (I : Inv es h out) {id : Nat}
    (hd : lookup h.calls id ≠ some .done) : results out id = 0 := by
  simp [I.count id, hd]

theorem not_mem_of_results_zero {out : List Effect} {id : Nat} {o : Outcome}
    (h : results out id = 0) : .result id o ∉ out := by
  intro hm
  have : 0 < results out id := List.countP_pos_iff.mpr ⟨_, hm, by simp [isResultFor]⟩
  omega

/-- A step that emits nothing and leaves the state alone keeps the invariant. -/
theorem inv_noop {es h out} (I : Inv es h out) (e : Event)
    (hk : ∀ id, e = .toolUse id → .toolUse id ∈ es) (hc : e = .cancel → .cancel ∈ es) :
    Inv (es ++ [e]) h (out ++ []) := by
  simp only [List.append_nil]
  refine ⟨I.nodup, I.count, fun id hr => List.mem_append_left _ (I.perm id hr), I.closed,
    ?_, ?_, I.fresh, I.asked, I.deny, I.askOpen, I.runAsk⟩
  · rw [I.flag]; simp only [List.mem_append, List.mem_singleton]
    exact ⟨Or.inl, fun h => h.elim (fun x => x) (fun h => hc h.symm)⟩
  · intro id; rw [I.known id]; simp only [List.mem_append, List.mem_singleton]
    exact ⟨Or.inl, fun h => h.elim (fun x => x) (fun h => hk id h.symm)⟩

/-- A step that moves call `id` from `p₀` to `p` by `upd`, emitting `new`. -/
theorem inv_upd {es h out} (I : Inv es h out) (e : Event) (id : Nat) (p₀ p : Phase)
    (new : List Effect) (hl : lookup h.calls id = some p₀) (hp₀ : p₀ ≠ .done)
    (hnotUse : ∀ i, e ≠ .toolUse i) (hnotCancel : e ≠ .cancel)
    (hres : results new id = if p = .done then 1 else 0)
    (hresOther : ∀ id', id' ≠ id → results new id' = 0)
    (hrun : ∀ id', .run id' ∈ new → id' = id ∧ p₀ = .asked ∧ e = .permission id true)
    (hdeny : ∀ id', .result id' .denied ∈ new → id' = id ∧ p₀ = .asked)
    (hpa : p ≠ .asked) (hboth : ∀ id', .run id' ∈ new → .result id' .denied ∉ new)
    (hask : ∀ id', .ask id' ∉ new) :
    Inv (es ++ [e]) ⟨upd h.calls id p, h.cancelled⟩ (out ++ new) := by
  have hs : (lookup h.calls id).isSome := by simp [hl]
  have hz : results out id = 0 := results_zero_of_not_done I (by simp [hl, hp₀])
  have hnotdone : h.cancelled = false := by
    cases hcan : h.cancelled
    · rfl
    · have := all_done_lookup (I.closed hcan) id; simp_all
  have hopen : Effect.ask id ∈ out := I.askOpen id (by cases p₀ <;> simp_all)
  refine ⟨by simpa [keys_upd] using I.nodup, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro id'
    rw [results_append, lookup_upd]
    by_cases h' : id' = id
    · subst h'; simp [hz, hs, hres]
    · simp [h', hresOther id' h', I.count id']
  · intro id' hr
    simp only [List.mem_append] at hr
    rcases hr with hr | hr
    · exact List.mem_append_left _ (I.perm id' hr)
    · obtain ⟨rfl, -, rfl⟩ := hrun id' hr; simp
  · intro hc; simp [hnotdone] at hc
  · rw [I.flag]; simp only [List.mem_append, List.mem_singleton]
    exact ⟨Or.inl, fun h => h.elim (fun x => x) (fun h => absurd h.symm hnotCancel)⟩
  · intro id'
    rw [lookup_upd]
    have hk' : Event.toolUse id' ∈ es ++ [e] ↔ Event.toolUse id' ∈ es := by
      simp only [List.mem_append, List.mem_singleton]
      exact ⟨fun h => h.elim (fun x => x) (fun h => absurd h.symm (hnotUse id')), Or.inl⟩
    rw [hk', ← I.known id']
    by_cases h' : id' = id
    · subst h'; simp [hs]
    · simp [h']
  · intro id' hn hr
    rw [lookup_upd] at hn
    have h' : id' ≠ id := by rintro rfl; simp [hs] at hn
    simp only [h', false_and, if_false] at hn
    simp only [List.mem_append] at hr
    rcases hr with hr | hr
    · exact I.fresh id' hn hr
    · exact h' (hrun id' hr).1
  · intro id' ha hr
    rw [lookup_upd] at ha
    by_cases h' : id' = id
    · subst h'; simp [hs] at ha; exact hpa ha
    · simp only [h', false_and, if_false] at ha
      simp only [List.mem_append] at hr
      rcases hr with hr | hr
      · exact I.asked id' ha hr
      · exact h' (hrun id' hr).1
  · intro id' hr hd
    simp only [List.mem_append] at hr hd
    rcases hr with hr | hr
    · rcases hd with hd | hd
      · exact I.deny id' hr hd
      · obtain ⟨rfl, rfl⟩ := hdeny id' hd; exact I.asked _ hl hr
    · obtain ⟨rfl, rfl, -⟩ := hrun id' hr
      rcases hd with hd | hd
      · exact not_mem_of_results_zero hz hd
      · exact hboth _ hr hd
  · intro id' ho
    rw [lookup_upd] at ho
    apply List.mem_append_left
    by_cases h' : id' = id
    · subst h'; exact hopen
    · simp only [h', false_and, if_false] at ho; exact I.askOpen id' ho
  · intro id' hr
    simp only [List.mem_append] at hr
    rcases hr with hr | hr
    · exact List.mem_append_left _ (I.runAsk id' hr)
    · obtain ⟨rfl, -, -⟩ := hrun id' hr; exact List.mem_append_left _ hopen

/-- A `toolUse` of a new id appends `(id, p)` and emits `new`. -/
theorem inv_use {es h out} (I : Inv es h out) (id : Nat) (hl : lookup h.calls id = none)
    (p : Phase) (c : Bool) (new : List Effect) (hc : c = h.cancelled)
    (hp : h.cancelled = true → p = .done)
    (hres : ∀ id', results new id' = if id' = id ∧ p = .done then 1 else 0)
    (hrun : ∀ id', .run id' ∉ new) (hdeny : ∀ id', .result id' .denied ∉ new)
    (hpk : p = .done ∨ .ask id ∈ new) :
    Inv (es ++ [.toolUse id]) ⟨h.calls ++ [(id, p)], c⟩ (out ++ new) := by
  subst hc
  have hz : results out id = 0 := results_zero_of_not_done I (by simp [hl])
  refine ⟨nodup_snoc I.nodup hl, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro id'
    rw [results_append, lookup_append, lookup_single, hres id']
    by_cases h' : id' = id
    · subst h'; simp [hl, hz]
    · have : ¬ id = id' := fun h => h' h.symm
      simp [h', this, I.count id']
  · intro id' hr
    simp only [List.mem_append] at hr
    rcases hr with hr | hr
    · exact List.mem_append_left _ (I.perm id' hr)
    · exact absurd hr (hrun id')
  · intro hcan c' hc'
    simp only [List.mem_append, List.mem_singleton] at hc'
    rcases hc' with hc' | rfl
    · exact I.closed hcan c' hc'
    · exact hp hcan
  · rw [I.flag]; simp
  · intro id'
    rw [lookup_append, lookup_single]
    simp only [List.mem_append, List.mem_singleton, Event.toolUse.injEq]
    by_cases h' : id = id'
    · subst h'; simp
    · simpa [h', Ne.symm h'] using I.known id'
  · intro id' hn hr
    rw [lookup_append, lookup_single] at hn
    simp only [List.mem_append] at hr
    by_cases h' : id = id'
    · subst h'; simp [hl] at hn
    · simp [h'] at hn
      rcases hr with hr | hr
      · exact I.fresh id' hn hr
      · exact hrun id' hr
  · intro id' ha hr
    rw [lookup_append, lookup_single] at ha
    simp only [List.mem_append] at hr
    by_cases h' : id = id'
    · subst h'
      rcases hr with hr | hr
      · exact I.fresh id hl hr
      · exact hrun id hr
    · simp [h'] at ha
      rcases hr with hr | hr
      · exact I.asked id' ha hr
      · exact hrun id' hr
  · intro id' hr hd
    simp only [List.mem_append] at hr hd
    rcases hr with hr | hr
    · rcases hd with hd | hd
      · exact I.deny id' hr hd
      · exact hdeny id' hd
    · exact hrun id' hr
  · intro id' ho
    rw [lookup_append, lookup_single] at ho
    by_cases h' : id = id'
    · subst h'
      simp only [hl, Option.none_or, if_true] at ho
      rcases hpk with hpk | hpk
      · subst hpk; simp at ho
      · exact List.mem_append_right _ hpk
    · simp only [h', if_false, Option.or_none] at ho
      exact List.mem_append_left _ (I.askOpen id' ho)
  · intro id' hr
    simp only [List.mem_append] at hr
    rcases hr with hr | hr
    · exact List.mem_append_left _ (I.runAsk id' hr)
    · exact absurd hr (hrun id')

theorem inv_step {es h out} (I : Inv es h out) (e : Event) :
    Inv (es ++ [e]) (step h e).1 (out ++ (step h e).2) := by
  cases e with
  | toolUse id =>
    simp only [step]
    split
    · next hl =>
      split
      · next hc =>
        exact inv_use I id hl .done true _ hc.symm (fun _ => rfl)
          (fun id' => by by_cases h' : id = id' <;> simp [h', eq_comm]) (by simp) (by simp) (.inl rfl)
      · next hc =>
        exact inv_use I id hl .asked false _ (by revert hc; cases h.cancelled <;> simp) (fun h => absurd h hc)
          (by simp) (by simp) (by simp) (.inr (by simp))
    · next p hl =>
      have hk : Event.toolUse id ∈ es := (I.known id).mp (by simp [hl])
      exact inv_noop I _ (fun _ h => by cases h; exact hk) (fun h => by cases h)
  | permission id allow =>
    simp only [step]
    split
    · next hl =>
      split
      · next ha =>
        subst ha
        exact inv_upd I _ id .asked .running [.run id] hl (by simp) (by simp) (by simp)
          (by simp) (by simp) (by simp) (by simp) (by simp) (by simp) (by simp)
      · next ha =>
        exact inv_upd I _ id .asked .done [.result id .denied] hl (by simp) (by simp) (by simp)
          (by simp) (fun id' h' => by simp [Ne.symm h']) (by simp) (by simp) (by simp) (by simp) (by simp)
    · exact inv_noop I _ (fun _ h => by cases h) (fun h => by cases h)
  | toolDone id =>
    simp only [step]
    split
    · next hl =>
      exact inv_upd I _ id .running .done [.result id .ok] hl (by simp) (by simp) (by simp)
        (by simp) (fun id' h' => by simp [Ne.symm h']) (by simp) (by simp) (by simp) (by simp) (by simp)
    · exact inv_noop I _ (fun _ h => by cases h) (fun h => by cases h)
  | cancel =>
    simp only [step]
    have hnr := cancel_no_run h.calls
    refine ⟨by rw [keys_cancel]; exact I.nodup, ?_, ?_, ?_, by simp, ?_, ?_, ?_, ?_, ?_, ?_⟩
    · intro id'
      rw [results_append, I.count id', results_cancel _ I.nodup, lookup_cancel]
      rcases lookup h.calls id' with _ | p
      · simp
      · cases p <;> simp
    · intro id' hr
      simp only [List.mem_append] at hr
      rcases hr with hr | hr
      · exact List.mem_append_left _ (I.perm id' hr)
      · exact absurd hr (hnr id').1
    · intro _ c hc
      simp only [List.mem_map] at hc
      obtain ⟨d, -, rfl⟩ := hc
      simp [cancelOne_fst]
    · intro id'; rw [lookup_cancel, Option.isSome_map, I.known id']; simp
    · intro id' hn
      rw [lookup_cancel, Option.map_eq_none_iff] at hn
      simp only [List.mem_append, not_or]
      exact ⟨I.fresh id' hn, (hnr id').1⟩
    · intro id' ha
      rw [lookup_cancel] at ha
      simp at ha
    · intro id' hr hd
      simp only [List.mem_append] at hr hd
      rcases hr with hr | hr
      · rcases hd with hd | hd
        · exact I.deny id' hr hd
        · exact (hnr id').2.2 hd
      · exact (hnr id').1 hr
    · intro id' ho
      rw [lookup_cancel] at ho
      simp at ho
    · intro id' hr
      simp only [List.mem_append] at hr
      rcases hr with hr | hr
      · exact List.mem_append_left _ (I.runAsk id' hr)
      · exact absurd hr (hnr id').1

theorem run_snoc (h : H) (es : List Event) (e : Event) :
    run h (es ++ [e]) = ((step (run h es).1 e).1, (run h es).2 ++ (step (run h es).1 e).2) := by
  induction es generalizing h with
  | nil => simp [run]
  | cons e' es ih => simp [run, ih]

theorem run_append (h : H) (a b : List Event) :
    run h (a ++ b) = ((run (run h a).1 b).1, (run h a).2 ++ (run (run h a).1 b).2) := by
  induction a generalizing h with
  | nil => simp [run]
  | cons e a ih => simp [run, ih]

theorem inv_run (es : List Event) : Inv es (run init es).1 (run init es).2 := by
  induction es using List.reverseRecOn with
  | nil => exact inv_init
  | append_singleton es e ih => rw [run_snoc]; exact inv_step ih e

/-! ## The five properties, for every trace -/

theorem permission_first (es : List Event) : PermissionFirst es (run init es).2 :=
  fun id hr => ⟨(inv_run es).runAsk id hr, (inv_run es).perm id hr⟩

theorem at_most_one_result (es : List Event) : AtMostOneResult (run init es).2 := by
  intro id; rw [(inv_run es).count id]; split <;> omega

theorem deny_final (es : List Event) : DenyFinal (run init es).2 :=
  fun id hd hr => (inv_run es).deny id hr hd

theorem closed_after_cancel (es : List Event) : ClosedAfterCancel es (run init es).2 := by
  intro hc id hu
  have I := inv_run es
  have hcan := I.flag.mpr hc
  obtain ⟨p, hp⟩ := Option.isSome_iff_exists.mp ((I.known id).mpr hu)
  have := all_done_lookup (I.closed hcan) id
  rw [I.count id]
  simp_all

/-- Once cancelled with every call done, a step stays that way and starts nothing. -/
theorem quiet_step (h : H) (hc : h.cancelled = true) (hd : ∀ c ∈ h.calls, c.2 = .done)
    (e : Event) :
    (step h e).1.cancelled = true ∧ (∀ c ∈ (step h e).1.calls, c.2 = .done) ∧
      ∀ id, .run id ∉ (step h e).2 ∧ .ask id ∉ (step h e).2 := by
  cases e with
  | toolUse id =>
    simp only [step, hc, if_true]
    split
    · refine ⟨rfl, fun c hc' => ?_, by simp⟩
      simp only [List.mem_append, List.mem_singleton] at hc'
      rcases hc' with hc' | rfl
      · exact hd c hc'
      · rfl
    · exact ⟨hc, hd, by simp⟩
  | permission id allow =>
    have := all_done_lookup hd id
    simp only [step]
    split
    · simp_all
    · exact ⟨hc, hd, by simp⟩
  | toolDone id =>
    have := all_done_lookup hd id
    simp only [step]
    split
    · simp_all
    · exact ⟨hc, hd, by simp⟩
  | cancel =>
    refine ⟨rfl, fun c hc' => ?_, fun id => ⟨(cancel_no_run h.calls id).1, (cancel_no_run h.calls id).2.1⟩⟩
    simp only [step, List.mem_map] at hc'
    obtain ⟨d, -, rfl⟩ := hc'
    simp [cancelOne_fst]

theorem quiet_run (h : H) (hc : h.cancelled = true) (hd : ∀ c ∈ h.calls, c.2 = .done)
    (post : List Event) : QuietAfterCancel h post := by
  induction post generalizing h with
  | nil => simp [QuietAfterCancel, run]
  | cons e post ih =>
    obtain ⟨hc', hd', hq⟩ := quiet_step h hc hd e
    intro id
    have := ih _ hc' hd' id
    simp only [run, List.mem_append, not_or]
    exact ⟨⟨(hq id).1, this.1⟩, ⟨(hq id).2, this.2⟩⟩

theorem quiet_after_cancel (pre post : List Event) :
    QuietAfterCancel (run init (pre ++ [.cancel])).1 post := by
  apply quiet_run
  · rw [run_snoc]; rfl
  · rw [run_snoc]
    intro c hc
    simp only [step, List.mem_map] at hc
    obtain ⟨d, -, rfl⟩ := hc
    simp [cancelOne_fst]

end Harness.Spec

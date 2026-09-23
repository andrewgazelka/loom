/-
The phase storage codec in `harness.rs` (`core::phase_code`, `core::phase_of_code`) is
a bijection onto {0, 1, 2}: the shell stores `phase_code p` and loads with
`phase_of_code`, so a stored call reads back as the same phase, and any other code is
refused instead of guessed.
-/
import Harness.Generated.Funs

open Aeneas Aeneas.Std Result
open harness

namespace Harness.Codec

theorem roundtrip (p : core.Phase) :
    (do let c ← core.phase_code p; core.phase_of_code c) = ok (some p) := by
  cases p <;> simp only [core.phase_code, core.phase_of_code, bind_tc_ok] <;> split <;> first | rfl | (exfalso; rename_i h; revert h; decide) | (exfalso; rename_i h0 _ _; exact h0 rfl) | (exfalso; rename_i _ h1 _; exact h1 rfl) | (exfalso; rename_i _ _ h2; exact h2 rfl)

theorem only_valid (c : U8) (p : core.Phase) (h : core.phase_of_code c = ok (some p)) :
    core.phase_code p = ok c := by
  unfold core.phase_of_code at h
  split at h <;> simp_all [core.phase_code] <;> subst h <;> rfl

end Harness.Codec

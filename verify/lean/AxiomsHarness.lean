import Harness
-- Build-enforced: each report must match exactly, so a new `sorry` (sorryAx) or a new
-- axiom anywhere under these theorems fails `lake env lean` and therefore check.sh.
-- The checker theorems (Harness.Check.*) use native_decide and are tests, not proofs.

/-- info: 'Harness.Spec.permission_first' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Spec.permission_first

/-- info: 'Harness.Spec.at_most_one_result' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Spec.at_most_one_result

/-- info: 'Harness.Spec.deny_final' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Spec.deny_final

/-- info: 'Harness.Spec.closed_after_cancel' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Spec.closed_after_cancel

/-- info: 'Harness.Spec.quiet_after_cancel' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Spec.quiet_after_cancel

/-- info: 'Harness.Refinement.step_refines' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Refinement.step_refines

/-- info: 'Harness.Refinement.rust_harness_correct' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Refinement.rust_harness_correct

/-- info: 'Harness.Refinement.rust_quiet_after_cancel' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Refinement.rust_quiet_after_cancel

/-- info: 'Harness.Codec.roundtrip' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Codec.roundtrip

/-- info: 'Harness.Codec.only_valid' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in
#print axioms Harness.Codec.only_valid

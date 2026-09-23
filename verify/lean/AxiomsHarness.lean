import Harness
-- No `sorryAx` may appear. `Lean.ofReduceBool` appears only for `native_decide`
-- (the checker run in Harness.Check), which trusts the compiler.
#print axioms Harness.Spec.permission_first
#print axioms Harness.Spec.at_most_one_result
#print axioms Harness.Spec.deny_final
#print axioms Harness.Spec.closed_after_cancel
#print axioms Harness.Spec.quiet_after_cancel
#print axioms Harness.Refinement.step_refines
#print axioms Harness.Refinement.rust_harness_correct
#print axioms Harness.Check.step_passes_depth3

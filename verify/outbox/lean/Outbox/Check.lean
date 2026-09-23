/-
A bounded model checker in Lean, the way TLC checks a TLA+ spec: enumerate every
event sequence up to a depth and report the first one that breaks the invariant.
Proofs in `Spec.lean` cover all depths; this is how you find the bug before you
know what to prove.
-/
import Outbox.Spec

namespace Outbox.Check
open Spec

def events (rows : Nat) : List Event :=
  .reset :: (List.range rows).flatMap fun i => [.pump i, .ack i]

/-- The property users observe: no key handled twice. Checking the stronger internal
`Inv` instead reports `[pump 0, reset]`, a state that breaks the invariant but has not
yet handled anything twice. -/
def Safe (r : Receiver) : Bool := decide r.handled.Nodup

/-- Breadth-first over traces; returns the shortest trace whose end state is not `Safe`. -/
def firstViolation (resetFn : Receiver → Receiver) (w : World) (depth : Nat) :
    Option (List Event) :=
  go depth [([], w)]
where
  go : Nat → List (List Event × World) → Option (List Event)
    | 0, _ => none
    | d + 1, frontier =>
      let next := frontier.flatMap fun (tr, s) =>
        (events w.outbox.length).map fun e => (tr ++ [e], step resetFn s e)
      match next.find? (fun (_, s) => !Safe s.receiver) with
      | some (tr, _) => some tr
      | none => go d next

-- Two rows, depth 5: 5^5 = 3125 traces.
#eval firstViolation badReset (fresh [7, 8]) 5   -- some [pump 0, reset, pump 0]
#eval firstViolation goodReset (fresh [7, 8]) 5  -- none

/-- The checker's verdict on the correct reset, checked by the kernel. -/
theorem good_no_violation_depth4 : firstViolation goodReset (fresh [7, 8]) 4 = none := by
  decide +kernel

theorem bad_violation_depth3 :
    firstViolation badReset (fresh [7, 8]) 3 = some [.pump 0, .reset, .pump 0] := by
  decide +kernel

end Outbox.Check

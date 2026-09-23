/-
A TLC-style checker that runs the Aeneas translation of `harness.rs` directly (the
code the proofs are about, not rustc output). It enumerates every interleaving of
model, user and tool events for two tool calls and reports the shortest trace that
breaks each property. The system under test is not a hand-written model; the
properties and the event/effect mappings below are hand-written.
-/
import Harness.Generated.Funs
import Harness.Spec

open Aeneas Aeneas.Std Result

namespace Harness.Check
open Spec

def u64 (n : Nat) : U64 := if n = 1 then 1#u64 else 2#u64

def toRust : Event → harness.core.Event
  | .toolUse id => .ToolUse (u64 id)
  | .permission id a u => .Permission (u64 id) a u
  | .toolDone id => .ToolDone (u64 id)
  | .cancel => .Cancel

def ofRustOutcome : harness.core.Outcome → Outcome
  | .Ok => .ok | .Denied => .denied | .Cancelled => .cancelled

def ofRust : harness.core.Effect → Effect
  | .AskPermission id => .ask id.val
  | .RunTool id => .run id.val
  | .AbortTool id => .abort id.val
  | .Result id o => .result id.val (ofRustOutcome o)

abbrev RustStep := harness.core.Harness → harness.core.Event →
  Result (alloc.vec.Vec harness.core.Effect × harness.core.Harness)

def ids : List Nat := [1, 2]

def events : List Event :=
  -- `.permission i _ false` is an answer forged by a non-user sender (the model, a tool).
  .cancel :: ids.flatMap fun i =>
    [.toolUse i, .permission i true true, .permission i false true, .permission i true false,
     .permission i false false, .toolDone i]

/-- A node: events so far, effects so far, effects emitted since the first cancel.
`failed` records a Rust step that did not return `ok` (a panic or overflow), which is
itself a violation: dropping such traces would let a crashing `step` pass. -/
structure Node where
  es : List Event
  out : List Effect
  sinceCancel : Option (List Effect)
  state : harness.core.Harness
  failed : Bool := false

def effId : Effect → Nat
  | .ask i | .run i | .abort i | .result i _ => i

/-- Position of the first occurrence, `none` when absent. -/
def firstIdx (out : List Effect) (e : Effect) : Option Nat := out.findIdx? (· == e)

def violations (n : Node) : List String :=
  -- every id the harness touched, not only the ones the events used
  let seen := (ids ++ n.out.map effId).eraseDups
  -- P1 with order: the prompt comes before the run
  let p1 := n.out.all fun e => match e with
    | .run id => n.es.contains (.permission id true true) &&
        match firstIdx n.out (.ask id), firstIdx n.out (.run id) with
        | some a, some r => a < r
        | _, _ => false
    | _ => true
  let p2 := seen.all fun id => results n.out id ≤ 1
  let p3 := seen.all fun id => !(n.out.contains (.result id .denied) && n.out.contains (.run id))
  let p4 := match n.sinceCancel with
    | none => true
    | some post => post.all fun e => match e with
      | .run _ => false
      | .ask _ => false
      | _ => true
  let p5 := !n.es.contains .cancel ||
    seen.all fun id => !n.es.contains (.toolUse id) || results n.out id == 1
  let checks : List (String × Bool) :=
    [("Rust step returned ok", !n.failed),
     ("P1 permission first", p1), ("P2 at most one result", p2), ("P3 deny final", p3),
     ("P4 quiet after cancel", p4), ("P5 closed after cancel", p5)]
  (checks.filter (fun c => !c.2)).map (·.1)

def expand (stepFn : RustStep) (n : Node) : List Node :=
  if n.failed then [] else
  events.map fun e =>
    -- `Result` is an interaction tree; `.match` exposes its head.
    match (stepFn n.state (toRust e)).match with
    | .ok (effs, s) =>
      let new := effs.val.map ofRust
      let since := match n.sinceCancel, e with
        | some post, _ => some (post ++ new)
        | none, .cancel => some []
        | none, _ => none
      ⟨n.es ++ [e], n.out ++ new, since, s, false⟩
    | _ => { n with es := n.es ++ [e], failed := true }

/-- Breadth-first: the first report of each property is its shortest counterexample. -/
def check (stepFn : RustStep) (depth : Nat) : List (String × List Event × List Effect) :=
  go depth [⟨[], [], none, ⟨alloc.vec.Vec.new _, false⟩, false⟩] []
where
  go : Nat → List Node → List (String × List Event × List Effect) →
      List (String × List Event × List Effect)
    | 0, _, found => found.reverse
    | d + 1, frontier, found =>
      let next := frontier.flatMap (expand stepFn)
      let found := next.foldl (init := found) fun acc n =>
        (violations n).foldl (init := acc) fun acc p =>
          if acc.any (·.1 == p) then acc else (p, n.es, n.out) :: acc
      go d next found

-- 13 events, depth 5: 13^5 = 371293 traces through the translated Rust step functions.
#eval check harness.core.step_v1 5
#eval check harness.core.step 5

/-- Gate: the fixed step has no violation in any of the 371293 traces. -/
theorem step_passes_depth5 : check harness.core.step 5 = [] := by native_decide

/-- Positive control: the checker still finds the first draft's bugs, with these
shortest traces. A checker that went blind fails this. -/
theorem step_v1_fails_depth3 :
    (check harness.core.step_v1 3).map (fun r => (r.1, r.2.1)) =
      [("P2 at most one result", [.toolUse 1, .cancel, .cancel]),
       ("P5 closed after cancel", [.toolUse 1, .cancel, .cancel]),
       ("P4 quiet after cancel", [.toolUse 1, .cancel, .permission 1 true true])] := by
  native_decide

end Harness.Check

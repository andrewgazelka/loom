/-
A TLC-style checker that runs the ACTUAL Rust code: `harness.core.step` here is the
Aeneas translation of `harness.rs`, executed directly. It enumerates every
interleaving of model, user and tool events for two tool calls and reports the
shortest trace that breaks each property. No hand-written model is involved, so
what it finds is a bug in the shipped code.
-/
import Harness.Generated.Funs
import Harness.Spec

open Aeneas Aeneas.Std Result

namespace Harness.Check
open Spec

def u64 (n : Nat) : U64 := if n = 1 then 1#u64 else 2#u64

def toRust : Event → harness.core.Event
  | .toolUse id => .ToolUse (u64 id)
  | .permission id a => .Permission (u64 id) a
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
  .cancel :: ids.flatMap fun i => [.toolUse i, .permission i true, .permission i false, .toolDone i]

/-- A node: events so far, effects so far, effects emitted since the first cancel. -/
structure Node where
  es : List Event
  out : List Effect
  sinceCancel : Option (List Effect)
  state : harness.core.Harness

def violations (n : Node) : List String :=
  let p1 := n.out.all fun e => match e with
    | .run id => n.es.contains (.permission id true)
    | _ => true
  let p2 := ids.all fun id => results n.out id ≤ 1
  let p3 := ids.all fun id => !(n.out.contains (.result id .denied) && n.out.contains (.run id))
  let p4 := match n.sinceCancel with
    | none => true
    | some post => post.all fun e => match e with
      | .run _ => false
      | .ask _ => false
      | _ => true
  let p5 := !n.es.contains .cancel ||
    ids.all fun id => !n.es.contains (.toolUse id) || results n.out id == 1
  let checks : List (String × Bool) :=
    [("P1 permission first", p1), ("P2 at most one result", p2), ("P3 deny final", p3),
     ("P4 quiet after cancel", p4), ("P5 closed after cancel", p5)]
  (checks.filter (fun c => !c.2)).map (·.1)

def expand (stepFn : RustStep) (n : Node) : List Node :=
  events.filterMap fun e =>
    -- `Result` is an interaction tree; `.match` exposes its head.
    match (stepFn n.state (toRust e)).match with
    | .ok (effs, s) =>
      let new := effs.val.map ofRust
      let since := match n.sinceCancel, e with
        | some post, _ => some (post ++ new)
        | none, .cancel => some []
        | none, _ => none
      some ⟨n.es ++ [e], n.out ++ new, since, s⟩
    | _ => none

/-- Breadth-first: the first report of each property is its shortest counterexample. -/
def check (stepFn : RustStep) (depth : Nat) : List (String × List Event × List Effect) :=
  go depth [⟨[], [], none, ⟨alloc.vec.Vec.new _, false⟩⟩] []
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

-- 9 events, depth 5: 9^5 = 59049 traces through the real Rust step functions.
#eval check harness.core.step_v1 5
#eval check harness.core.step 5

theorem step_passes_depth3 : check harness.core.step 3 = [] := by native_decide

end Harness.Check

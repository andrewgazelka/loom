/// A definition calling itself through the host: `$self` names the running
/// definition. Each level is a fresh isolated instance; the host stops the
/// recursion at `loom::isolated::MAX_DEPTH` with `CallError::DepthExceeded`.
const DESCEND: loom::isolated::Def<fn(u32) -> u32> = loom::isolated::Def::new("$self");

pub fn descend(depth: u32) -> u32 {
    if depth == 0 {
        0
    } else {
        loom::isolated::call(DESCEND, depth - 1).expect("child call failed") + 1
    }
}

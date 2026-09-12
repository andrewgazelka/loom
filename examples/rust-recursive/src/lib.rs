/// A definition calling itself through the host: `$self` names the running definition.
const DESCEND: loom::Def<fn(u32) -> u32> = loom::Def::new("$self");

pub fn descend(depth: u32) -> u32 {
    if depth == 0 {
        0
    } else {
        loom::call(DESCEND, depth - 1).expect("child call failed") + 1
    }
}

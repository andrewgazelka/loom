#[loom::def(effects=["call"])]
pub fn descend(depth: u32) -> u32 {
    if depth == 0 {
        0
    } else {
        loom::call(DESCEND_DEF, depth - 1).expect("child call failed") + 1
    }
}

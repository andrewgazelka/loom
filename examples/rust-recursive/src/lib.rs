#[loom::def]
pub fn descend(depth: u32) -> u32 {
    if depth == 0 {
        return 0;
    }
    let child = loom::fork(DESCEND_DEF, depth - 1).expect("child fork failed");
    let result = loom::join([child]).expect("child execution failed");
    result[0] + 1
}

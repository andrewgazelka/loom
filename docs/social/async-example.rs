use loom::abilities::sleep;

#[loom::def]
pub fn main() {
    loom::all([
        sleep::desc(100),
        sleep::desc(200),
    ]).expect("sleep failed");
}

use loom::{scope, sleep};

#[loom::def(effects = ["sleep"])]
pub fn main() {
    scope(|s| {
        s.spawn(|| sleep(100)).unwrap();
        s.spawn(|| sleep(200)).unwrap();
    });
}

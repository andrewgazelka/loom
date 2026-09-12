use loom::{scope, sleep};

pub fn main() {
    scope(|s| {
        s.spawn(|| sleep(100)).unwrap();
        s.spawn(|| sleep(200)).unwrap();
    });
}

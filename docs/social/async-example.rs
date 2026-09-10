use loom::abilities::fs;

fn read(path: &str) -> String {
    fs::read("local", path).unwrap()
}

#[loom::def]
pub fn main() -> Vec<String> {
    loom::scope(|s| {
        let a = s.fork(|| read("a.txt")).unwrap();
        let b = read("b.txt");
        vec![a.join().unwrap(), b]
    }).unwrap()
}

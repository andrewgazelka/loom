use loom::abilities::sleep;

#[loom::def]
pub fn main() {
    loom::scope(|s| {
        let job = s.fork(|| sleep(1_000).unwrap()).unwrap();

        sleep(1_000).unwrap();
        job.join().unwrap();
    }).unwrap();
}

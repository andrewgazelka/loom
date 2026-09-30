#[allow(dead_code)]
mod bracket {
    include!("../../gen/gen_core.rs");
}
use std::time::Instant;

fn main() {
    let n: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    // one-set bytes for exact comparison, then the sweep
    let p = bracket::params(0);
    let bytes = bracket::bracket(p[0], p[1], p[2], p[3], p[4]).unwrap();
    println!("set0 bytes {} fnv {:016x}", bytes.len(), bracket::fnv(&bytes));
    let mut best = f64::MAX;
    let mut hashes = Vec::new();
    for _ in 0..9 {
        let t = Instant::now();
        hashes = bracket::sweep(0, n);
        best = best.min(t.elapsed().as_secs_f64());
    }
    let refused = hashes.iter().filter(|h| **h == 0).count();
    let mut all = 0xcbf29ce484222325u64;
    for h in &hashes {
        all = bracket::fnv(&[all.to_le_bytes(), h.to_le_bytes()].concat());
    }
    println!("sweep {n} sets: best {:.3} ms total, {:.3} us per call, refused {refused}, combined fnv {all:016x}", best * 1e3, best * 1e6 / n as f64);
    for i in [0usize, 1, 500, 999] {
        if i < hashes.len() {
            println!("set{i} fnv {:016x}", hashes[i]);
        }
    }
}

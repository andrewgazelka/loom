use super::*;
use std::num::NonZeroUsize;

const MS: u64 = 1_000_000;
const K: [u8; 32] = [7; 32];

#[test]
fn a_result_is_found_by_callee_entry_arity_and_arguments_only() {
    let cache = ResultCache::default();
    assert!(cache.get("f", "main", 1, b"x", &K).is_none());
    cache.put("f", "main", 1, b"x", &K, b"42", MS);
    assert_eq!(cache.get("f", "main", 1, b"x", &K).unwrap(), b"42");
    for (callee, entry, argc, payload) in [
        ("g", "main", 1, &b"x"[..]),
        ("f", "other", 1, b"x"),
        ("f", "main", 2, b"x"),
        ("f", "main", 1, b"y"),
    ] {
        assert!(cache.get(callee, entry, argc, payload, &K).is_none());
    }
    let stats = cache.stats();
    assert_eq!((stats.hits, stats.misses, stats.stores, stats.entries), (1, 5, 1, 1));
    assert_eq!(stats.saved_ns, MS);
}

#[test]
fn a_computation_cheaper_than_a_lookup_is_not_stored() {
    let cache = ResultCache::default();
    // 1 microsecond to compute, against ~3 microseconds to look up.
    cache.put("cheap", "main", 0, b"a", &K, b"1", 1_000);
    assert!(cache.get("cheap", "main", 0, b"a", &K).is_none());
    // The same result at 100 microseconds is kept.
    cache.put("dear", "main", 0, b"a", &K, b"1", 100_000);
    assert!(cache.get("dear", "main", 0, b"a", &K).is_some());
    // A big result raises the bar: 300 KB costs ~30 microseconds to copy out.
    assert!(!worth_storing(100_000, 300_000));
    assert!(worth_storing(200_000, 300_000));
    let stats = cache.stats();
    assert_eq!((stats.skipped_cheap, stats.stores), (1, 1));
    let by = cache.by_callee();
    let cheap = by.iter().find(|(name, _)| name == "cheap").unwrap();
    assert_eq!((cheap.1.computed, cheap.1.skipped_cheap, cheap.1.stored), (1, 1, 0));
}

#[test]
fn eviction_drops_the_result_worth_least_per_byte() {
    // Room for two 400-byte results.
    let cache = ResultCache::with_capacity(900);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], 20 * MS); // 50,000 ns per byte
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], MS / 5); // 500 ns per byte
    cache.put("f", "main", 0, &payload(3), &K, &[0; 400], 10 * MS); // forces one out
    assert!(cache.get("f", "main", 0, &payload(1), &K).is_some(), "the 20 ms result stays");
    assert!(cache.get("f", "main", 0, &payload(2), &K).is_none(), "the 0.2 ms result goes");
    assert!(cache.get("f", "main", 0, &payload(3), &K).is_some());
    assert_eq!(cache.stats().evictions, 1);
    assert!(cache.stats().bytes <= 900);
}

#[test]
fn an_entry_that_keeps_hitting_outlives_a_costlier_one_that_never_does() {
    let cache = ResultCache::with_capacity(900);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], MS); // cheaper, but used
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], 4 * MS); // dearer, never asked again
    for _ in 0..8 {
        assert!(cache.get("f", "main", 0, &payload(1), &K).is_some());
    }
    cache.put("f", "main", 0, &payload(3), &K, &[0; 400], 2 * MS);
    assert!(cache.get("f", "main", 0, &payload(1), &K).is_some(), "eight hits outweigh a 4x cost");
    assert!(cache.get("f", "main", 0, &payload(2), &K).is_none());
}

#[test]
fn clearing_one_callee_leaves_the_others_and_clearing_all_empties() {
    let cache = ResultCache::default();
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    cache.put("f", "main", 0, b"b", &K, b"2", MS);
    cache.put("g", "main", 0, b"a", &K, b"3", MS);
    assert_eq!(cache.clear(Some("f")), 2);
    assert!(cache.get("f", "main", 0, b"a", &K).is_none());
    assert_eq!(cache.get("g", "main", 0, b"a", &K).unwrap(), b"3");
    assert_eq!(cache.clear(None), 1);
    let stats = cache.stats();
    assert_eq!((stats.entries, stats.bytes), (0, 0));
}

#[test]
fn an_oversized_result_is_not_kept() {
    let cache = ResultCache::default();
    cache.put("f", "main", 0, b"big", &K, &vec![0; MAX_RESULT_BYTES + 1], 10_000 * MS);
    assert_eq!(cache.stats().entries, 0);
}

/// A byte-bounded LRU that stores everything, the baseline a cost-blind cache is.
struct Lru {
    map: lru::LruCache<u32, usize>,
    bytes: usize,
    max: usize,
    skip_cheap: bool,
}

impl Lru {
    fn new(max: usize, skip_cheap: bool) -> Self {
        Self { map: lru::LruCache::new(NonZeroUsize::new(1 << 20).unwrap()), bytes: 0, max, skip_cheap }
    }
    fn get(&mut self, key: u32) -> bool {
        self.map.get(&key).is_some()
    }
    fn put(&mut self, key: u32, size: usize, cost_ns: u64) {
        if size > self.max || (self.skip_cheap && !worth_storing(cost_ns, size)) {
            return;
        }
        while self.bytes + size > self.max {
            let Some((_, old)) = self.map.pop_lru() else { break };
            self.bytes -= old;
        }
        self.map.put(key, size);
        self.bytes += size;
    }
}

/// Deterministic workload: 4000 distinct calls with a skewed popularity, costs from
/// 20 microseconds to 80 milliseconds and sizes from 200 bytes to 400 KB, drawn
/// independently, 40,000 requests, 16 MB of cache. Prints what each policy saved.
///
/// Run: `cargo test -p loom-rt --release result_cache -- --nocapture cost_aware`.
#[test]
fn cost_aware_eviction_saves_more_compute_than_lru_at_the_same_memory() {
    let mut state = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    const KEYS: usize = 4000;
    let log_uniform = |u: f64, lo: f64, hi: f64| (lo.ln() + u * (hi.ln() - lo.ln())).exp();
    let calls: Vec<(u64, usize)> = (0..KEYS)
        .map(|_| {
            let cost = log_uniform(next(), 20_000.0, 80_000_000.0) as u64;
            let size = log_uniform(next(), 200.0, 400_000.0) as usize;
            (cost, size)
        })
        .collect();
    // Zipf(1.0) popularity by inverse-CDF over ranks.
    let weights: Vec<f64> = (1..=KEYS).map(|rank| 1.0 / rank as f64).collect();
    let total: f64 = weights.iter().sum();
    let mut cumulative = Vec::with_capacity(KEYS);
    let mut running = 0.0;
    for weight in &weights {
        running += weight / total;
        cumulative.push(running);
    }
    let requests: Vec<u32> = (0..40_000)
        .map(|_| {
            let u = next();
            cumulative.partition_point(|&c| c < u).min(KEYS - 1) as u32
        })
        .collect();
    let never_cached: u64 = requests.iter().map(|&k| calls[k as usize].0).sum();
    let budget = 16 * 1024 * 1024;
    let payload = |k: u32| k.to_le_bytes();

    let mut report = Vec::new();
    for (name, skip_cheap) in [("LRU, stores everything", false), ("LRU, skips cheap", true)] {
        let mut cache = Lru::new(budget, skip_cheap);
        let (mut hits, mut saved) = (0u64, 0u64);
        for &k in &requests {
            let (cost, size) = calls[k as usize];
            if cache.get(k) {
                hits += 1;
                saved += cost;
            } else {
                cache.put(k, size, cost);
            }
        }
        report.push((name, hits, saved));
    }
    let cache = ResultCache::with_capacity(budget);
    let (mut hits, mut saved) = (0u64, 0u64);
    let mut stored_bytes = vec![0u8; 400_000];
    for &k in &requests {
        let (cost, size) = calls[k as usize];
        if cache.get("f", "main", 0, &payload(k), &K).is_some() {
            hits += 1;
            saved += cost;
        } else {
            stored_bytes.resize(size, 0);
            cache.put("f", "main", 0, &payload(k), &K, &stored_bytes, cost);
        }
    }
    report.push(("GDSF, skips cheap", hits, saved));

    println!("workload: {KEYS} distinct calls, {} requests, {} MB cache, {:.1} s of compute if nothing is cached", requests.len(), budget >> 20, never_cached as f64 / 1e9);
    for (name, hits, saved) in &report {
        println!(
            "{name:26} hit rate {:5.1}%   compute saved {:5.1}%  ({:.2} s)",
            *hits as f64 * 100.0 / requests.len() as f64,
            *saved as f64 * 100.0 / never_cached as f64,
            *saved as f64 / 1e9
        );
    }
    let saved = |name: &str| report.iter().find(|row| row.0 == name).unwrap().2;
    assert!(saved("GDSF, skips cheap") >= saved("LRU, stores everything"));
    assert!(cache.stats().bytes <= budget);
}

#[test]
fn a_different_kernel_fingerprint_is_a_different_key() {
    let cache = ResultCache::default();
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    assert!(cache.get("f", "main", 0, b"a", &K).is_some());
    assert!(
        cache.get("f", "main", 0, b"a", &[8; 32]).is_none(),
        "a result made under other kernel versions is not served"
    );
}

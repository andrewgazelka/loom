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
    assert_eq!(
        (stats.hits, stats.misses, stats.stores, stats.entries),
        (1, 5, 1, 1)
    );
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
    assert_eq!(
        (cheap.1.computed, cheap.1.skipped_cheap, cheap.1.stored),
        (1, 1, 0)
    );
}

#[test]
fn eviction_drops_the_result_worth_least_per_byte() {
    // Room for two 400-byte results.
    let cache = ResultCache::with_capacity(900);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], 20 * MS); // 50,000 ns per byte
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], MS / 5); // 500 ns per byte
    cache.put("f", "main", 0, &payload(3), &K, &[0; 400], 10 * MS); // forces one out
    assert!(
        cache.get("f", "main", 0, &payload(1), &K).is_some(),
        "the 20 ms result stays"
    );
    assert!(
        cache.get("f", "main", 0, &payload(2), &K).is_none(),
        "the 0.2 ms result goes"
    );
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
    assert!(
        cache.get("f", "main", 0, &payload(1), &K).is_some(),
        "eight hits outweigh a 4x cost"
    );
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
    cache.put(
        "f",
        "main",
        0,
        b"big",
        &K,
        &vec![0; MAX_RESULT_BYTES + 1],
        10_000 * MS,
    );
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
        Self {
            map: lru::LruCache::new(NonZeroUsize::new(1 << 20).unwrap()),
            bytes: 0,
            max,
            skip_cheap,
        }
    }
    fn get(&mut self, key: u32) -> bool {
        self.map.get(&key).is_some()
    }
    fn put(&mut self, key: u32, size: usize, cost_ns: u64) {
        if size > self.max || (self.skip_cheap && !worth_storing(cost_ns, size)) {
            return;
        }
        while self.bytes + size > self.max {
            let Some((_, old)) = self.map.pop_lru() else {
                break;
            };
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
    for (name, skip_cheap) in [
        ("LRU, stores everything", false),
        ("LRU, skips cheap", true),
    ] {
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

    println!(
        "workload: {KEYS} distinct calls, {} requests, {} MB cache, {:.1} s of compute if nothing is cached",
        requests.len(),
        budget >> 20,
        never_cached as f64 / 1e9
    );
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

fn reopen(dir: &tempfile::TempDir, max_bytes: usize) -> ResultCache {
    ResultCache::persistent(max_bytes, dir.path())
}

#[test]
fn results_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = reopen(&dir, MAX_BYTES);
    first.put("f", "main", 1, b"x", &K, b"42", MS);
    first.put("g", "main", 0, b"y", &K, b"43", MS);
    drop(first); // flushes and joins the writer
    let second = reopen(&dir, MAX_BYTES);
    assert_eq!(second.get("f", "main", 1, b"x", &K).unwrap(), b"42");
    assert_eq!(second.get("g", "main", 0, b"y", &K).unwrap(), b"43");
    assert!(second.get("f", "main", 1, b"other", &K).is_none());
    let stats = second.stats();
    assert_eq!(
        (
            stats.loaded_at_start,
            stats.persisted_entries,
            stats.entries
        ),
        (2, 2, 2)
    );
    assert_eq!(stats.bytes, 4);
    // A hit on a loaded result saves what its computation cost the first time.
    assert_eq!(stats.saved_ns, 2 * MS);
}

#[test]
fn a_cache_without_a_directory_stays_in_memory() {
    let cache = ResultCache::open(None);
    cache.put("f", "main", 0, b"x", &K, b"1", MS);
    cache.flush_persisted();
    let stats = cache.stats();
    assert_eq!(
        (
            stats.entries,
            stats.persisted_entries,
            stats.loaded_at_start
        ),
        (1, 0, 0)
    );
}

#[test]
fn eviction_removes_the_persisted_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, 900);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], 20 * MS);
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], MS / 5);
    cache.flush_persisted();
    assert_eq!(cache.stats().persisted_entries, 2);
    cache.put("f", "main", 0, &payload(3), &K, &[0; 400], 10 * MS); // evicts payload 2
    cache.flush_persisted();
    let stats = cache.stats();
    assert_eq!(
        (stats.evictions, stats.entries, stats.persisted_entries),
        (1, 2, 2)
    );
    drop(cache);
    let again = reopen(&dir, 900);
    assert!(again.get("f", "main", 0, &payload(1), &K).is_some());
    assert!(
        again.get("f", "main", 0, &payload(2), &K).is_none(),
        "the evicted result does not return"
    );
    assert!(again.get("f", "main", 0, &payload(3), &K).is_some());
}

#[test]
fn clearing_removes_persisted_rows_including_ones_not_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    cache.put("f", "main", 0, b"b", &K, b"2", MS);
    cache.put("g", "main", 0, b"a", &K, b"3", MS);
    assert_eq!(cache.clear(Some("f")), 2);
    cache.flush_persisted();
    assert_eq!(cache.stats().persisted_entries, 1);
    drop(cache);
    let second = reopen(&dir, MAX_BYTES);
    assert!(second.get("f", "main", 0, b"a", &K).is_none());
    assert_eq!(second.get("g", "main", 0, b"a", &K).unwrap(), b"3");
    assert_eq!(second.clear(None), 1);
    drop(second);
    let third = reopen(&dir, MAX_BYTES);
    assert_eq!(
        (third.stats().entries, third.stats().loaded_at_start),
        (0, 0)
    );
}

#[test]
fn a_corrupt_file_is_replaced_and_the_cache_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("cache.db"),
        b"this is not a sqlite database, at all",
    )
    .unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    assert_eq!(
        (cache.stats().entries, cache.stats().loaded_at_start),
        (0, 0)
    );
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    drop(cache);
    let again = reopen(&dir, MAX_BYTES);
    assert_eq!(
        again.get("f", "main", 0, b"a", &K).unwrap(),
        b"1",
        "the replacement file works"
    );
}

#[test]
fn a_file_of_an_older_format_version_is_replaced_and_a_newer_one_is_left_alone() {
    let set_version = |dir: &tempfile::TempDir, version: i64| {
        let db = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
        db.execute_batch(&format!("PRAGMA user_version={version}"))
            .unwrap();
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    drop(cache);
    set_version(&dir, 1);
    let again = reopen(&dir, MAX_BYTES);
    assert_eq!(again.stats().loaded_at_start, 0);
    assert!(again.get("f", "main", 0, b"a", &K).is_none());
    drop(again);
    assert_eq!(
        rows_in_file(&dir),
        0,
        "the older file was replaced by an empty one"
    );

    // A newer build's file is not ours to delete: this build runs without persistence and the
    // rows stay for the build that wrote them.
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    drop(cache);
    set_version(&dir, 99);
    let older_build = reopen(&dir, MAX_BYTES);
    assert_eq!(older_build.stats().loaded_at_start, 0);
    older_build.put("g", "main", 0, b"a", &K, b"2", MS);
    assert_eq!(
        older_build.get("g", "main", 0, b"a", &K).unwrap(),
        b"2",
        "memory still works"
    );
    drop(older_build);
    assert_eq!(rows_in_file(&dir), 1, "the newer file still holds its row");
    let version: i64 = rusqlite::Connection::open(dir.path().join("cache.db"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 99);
}

#[test]
fn a_result_over_one_mib_is_served_from_memory_but_not_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    let big = vec![9u8; persist::MAX_VALUE_BYTES + 1];
    let fits = vec![9u8; persist::MAX_VALUE_BYTES];
    cache.put("f", "main", 0, b"big", &K, &big, 10_000 * MS);
    cache.put("f", "main", 0, b"fits", &K, &fits, 10_000 * MS);
    assert_eq!(cache.get("f", "main", 0, b"big", &K).unwrap(), big);
    cache.flush_persisted();
    let stats = cache.stats();
    assert_eq!((stats.entries, stats.persisted_entries), (2, 1));
    drop(cache);
    let again = reopen(&dir, MAX_BYTES);
    assert!(again.get("f", "main", 0, b"big", &K).is_none());
    assert_eq!(again.get("f", "main", 0, b"fits", &K).unwrap(), fits);
}

#[test]
fn a_different_kernel_fingerprint_does_not_hit_a_persisted_result() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    drop(cache);
    let again = reopen(&dir, MAX_BYTES);
    assert!(again.get("f", "main", 0, b"a", &[8; 32]).is_none());
    assert!(again.get("f", "main", 0, b"a", &K).is_some());
}

#[test]
fn a_smaller_cache_keeps_the_rows_worth_most_per_byte_and_trims_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], MS / 5); // 500 ns per byte
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], 20 * MS); // 50,000 ns per byte
    drop(cache);
    let small = reopen(&dir, 500);
    assert_eq!(
        (
            small.stats().loaded_at_start,
            small.stats().persisted_entries
        ),
        (1, 1)
    );
    assert!(small.get("f", "main", 0, &payload(2), &K).is_some());
    assert!(small.get("f", "main", 0, &payload(1), &K).is_none());
    drop(small);
    let big = reopen(&dir, MAX_BYTES);
    assert_eq!(
        big.stats().loaded_at_start,
        1,
        "the trimmed row is gone from the file"
    );
}

#[test]
fn hits_are_written_in_the_batch_and_change_what_a_smaller_cache_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    let payload = |n: u8| [n];
    cache.put("f", "main", 0, &payload(1), &K, &[0; 400], MS); // cheaper, but used
    cache.put("f", "main", 0, &payload(2), &K, &[0; 400], 4 * MS); // dearer, never asked again
    for _ in 0..8 {
        assert!(cache.get("f", "main", 0, &payload(1), &K).is_some());
    }
    drop(cache); // pending hit bumps are written on shutdown
    let small = reopen(&dir, 500);
    assert!(
        small.get("f", "main", 0, &payload(1), &K).is_some(),
        "nine hits outweigh a 4x cost"
    );
    assert!(small.get("f", "main", 0, &payload(2), &K).is_none());
}

fn rows_in_file(dir: &tempfile::TempDir) -> i64 {
    rusqlite::Connection::open(dir.path().join("cache.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM results", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn another_host_identity_is_another_key_and_its_rows_are_kept_at_load() {
    let (a, b) = ([1u8; 32], [2u8; 32]);
    assert_ne!(
        key(&a, "f", "main", 0, b"x", &K).digest,
        key(&b, "f", "main", 0, b"x", &K).digest,
        "a rebuilt host must not match a previous build's key"
    );
    let dir = tempfile::tempdir().unwrap();
    let first = ResultCache::persistent_as(MAX_BYTES, dir.path(), a);
    first.put("f", "main", 0, b"x", &K, b"42", MS);
    drop(first);
    assert_eq!(rows_in_file(&dir), 1);
    let same = ResultCache::persistent_as(MAX_BYTES, dir.path(), a);
    assert_eq!(same.get("f", "main", 0, b"x", &K).unwrap(), b"42");
    drop(same);
    let other = ResultCache::persistent_as(MAX_BYTES, dir.path(), b);
    assert_eq!(other.stats().loaded_at_start, 0);
    assert!(other.get("f", "main", 0, b"x", &K).is_none());
    drop(other);
    assert_eq!(
        rows_in_file(&dir),
        1,
        "the other build's row is not ours to delete"
    );
    let back = ResultCache::persistent_as(MAX_BYTES, dir.path(), a);
    assert_eq!(
        back.get("f", "main", 0, b"x", &K).unwrap(),
        b"42",
        "and its own build still hits it"
    );
}

#[test]
fn another_identitys_rows_are_trimmed_only_past_the_byte_cap_and_the_worst_go_first() {
    let (a, b) = ([1u8; 32], [2u8; 32]);
    let dir = tempfile::tempdir().unwrap();
    let first = ResultCache::persistent_as(MAX_BYTES, dir.path(), a);
    // Three 2-byte rows, the middle one worth the most per byte.
    first.put("cheap", "main", 0, b"x", &K, b"aa", MS);
    first.put("dear", "main", 0, b"x", &K, b"bb", 100 * MS);
    first.put("cheap2", "main", 0, b"x", &K, b"cc", MS);
    drop(first);
    assert_eq!(rows_in_file(&dir), 3);
    // Room for 4 bytes in all: two of the three rows stay, and `dear` is one of them.
    let other = ResultCache::persistent_as(4, dir.path(), b);
    drop(other);
    assert_eq!(rows_in_file(&dir), 2);
    let back = ResultCache::persistent_as(MAX_BYTES, dir.path(), a);
    assert!(
        back.get("dear", "main", 0, b"x", &K).is_some(),
        "the worst rows went first"
    );
}

#[test]
fn a_value_that_does_not_match_its_checksum_is_dropped_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"42", MS);
    cache.put("g", "main", 0, b"a", &K, b"43", MS);
    drop(cache);
    // Same length, other bytes: the size check alone would accept it.
    rusqlite::Connection::open(dir.path().join("cache.db"))
        .unwrap()
        .execute("UPDATE results SET value = x'3434' WHERE callee = 'f'", [])
        .unwrap();
    let again = reopen(&dir, MAX_BYTES);
    assert_eq!(again.stats().loaded_at_start, 1);
    assert!(again.get("f", "main", 0, b"a", &K).is_none());
    assert_eq!(again.get("g", "main", 0, b"a", &K).unwrap(), b"43");
    drop(again);
    assert_eq!(rows_in_file(&dir), 1, "the damaged row was deleted");
}

#[test]
fn a_clear_that_fails_is_retried_before_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    cache.put("f", "main", 0, b"a", &K, b"1", MS);
    cache.put("g", "main", 0, b"a", &K, b"2", MS);
    cache.flush_persisted();
    assert_eq!(cache.stats().persisted_entries, 2);
    // Another connection makes every delete fail, as a full disk or a lock would.
    let db = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse BEFORE DELETE ON results BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    assert_eq!(cache.clear(None), 2);
    cache.flush_persisted();
    assert_eq!(
        cache.stats().persisted_entries,
        2,
        "the failed clear changed nothing on disk"
    );
    db.execute_batch("DROP TRIGGER refuse").unwrap();
    cache.flush_persisted(); // the writer retries the clear with this batch
    assert_eq!(cache.stats().persisted_entries, 0);
    drop(cache);
    drop(db);
    let again = reopen(&dir, MAX_BYTES);
    assert_eq!(
        (again.stats().entries, again.stats().loaded_at_start),
        (0, 0)
    );
}

#[test]
fn queued_values_are_bounded_by_bytes_and_released_once_written() {
    let budget = persist::ByteBudget::new(10);
    assert!(budget.reserve(6));
    assert!(
        !budget.reserve(5),
        "a store that would pass the limit is refused"
    );
    assert_eq!(budget.used(), 6);
    budget.release(6);
    assert!(budget.reserve(10));
    assert!(!budget.reserve(1));

    let dir = tempfile::tempdir().unwrap();
    let cache = reopen(&dir, MAX_BYTES);
    let value = vec![5u8; 256 << 10];
    for n in 0..8u8 {
        cache.put("f", "main", 0, &[n], &K, &value, 1_000 * MS);
    }
    cache.flush_persisted();
    let queued = cache.persist.as_ref().unwrap().queued_bytes();
    assert_eq!(queued, 0, "the writer released what it wrote");
    assert_eq!(cache.stats().persisted_entries, 8);
}

#[test]
fn only_corruption_and_format_mismatch_justify_deleting_the_file() {
    let failure = |code| {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
        .context("pragmas")
    };
    assert!(persist::is_corrupt(&failure(rusqlite::ffi::SQLITE_CORRUPT)));
    assert!(persist::is_corrupt(&failure(rusqlite::ffi::SQLITE_NOTADB)));
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_CANTOPEN,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_READONLY,
    ] {
        assert!(
            !persist::is_corrupt(&failure(code)),
            "code {code} must leave the file alone"
        );
    }
    assert!(!persist::is_corrupt(&anyhow::anyhow!("something else")));
}

#[test]
fn callee_statistics_are_capped_and_keep_the_callees_that_saved_most() {
    let cache = ResultCache::default();
    cache.put("hot", "main", 0, b"a", &K, b"1", MS);
    for _ in 0..3 {
        assert!(cache.get("hot", "main", 0, b"a", &K).is_some());
    }
    for n in 0..MAX_CALLEES + 100 {
        cache.put(&format!("callee-{n}"), "main", 0, b"a", &K, b"1", MS);
    }
    assert!(cache.inner.lock().unwrap().callees.len() <= MAX_CALLEES);
    let top = cache.by_callee();
    assert_eq!(top.len(), TOP_CALLEES);
    assert_eq!(top[0].0, "hot");
    assert_eq!(top[0].1.saved_ns, 3 * MS);
    assert!(
        top.windows(2)
            .all(|pair| pair[0].1.saved_ns >= pair[1].1.saved_ns)
    );
    assert_eq!(
        cache.stats().saved_ns,
        3 * MS,
        "the total does not depend on which callees are kept"
    );
}

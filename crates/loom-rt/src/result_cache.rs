//! Results of isolated calls, remembered by what determines them, kept by what
//! they are worth.
//!
//! A call to another definition is a pure function of three things: the callee's
//! definition hash (its code and its dependencies, so an edit changes the key and
//! nothing stale can match), the entry, and the argument payload. When the callee
//! can do nothing else, running it again is wasted work, so the host returns the
//! bytes it returned last time.
//!
//! "Can do nothing else" is checked twice. Statically, the entry's effect row must
//! be empty and fully known. At run time, the call must have recorded no effect in
//! the execution trace (the static row can undercount: an effect reached only
//! through a `Display` impl is not in it). A call that fails, or that touched an
//! effect, is never stored.
//!
//! What is worth keeping is measured, not guessed. Each stored result carries the
//! time its computation took (`cost_ns`) and its size. Two rules follow:
//!
//! * **Skip what is cheaper to recompute than to look up.** A lookup hashes the
//!   payload, takes a lock and copies the result; a computation that costs no more
//!   than a few times that is not stored ([`worth_storing`]).
//! * **Evict by benefit per byte.** GreedyDual-Size-Frequency (Cherkasova 1998):
//!   an entry's priority is `L + hits * cost / size`, where `L` is the priority of
//!   the last entry evicted. The lowest priority goes first, so a 20 ms result of
//!   2 KB outlives a 0.2 ms result of 500 KB, and an entry that keeps hitting keeps
//!   its place; `L` rising ages out entries that stop being used.
//!
//! The cache is in memory and bounded in bytes; a restart empties it, which is
//! always safe.
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Bytes of results kept.
const MAX_BYTES: usize = 128 * 1024 * 1024;
/// A result larger than this is not worth the memory whatever it cost.
const MAX_RESULT_BYTES: usize = 8 << 20;

/// A lookup costs about this much before the copy: a payload hash, a lock and a
/// map probe (measured at 1 to 3 microseconds).
const LOOKUP_BASE_NS: u64 = 3_000;
/// Copying a result out costs about this per byte (memory bandwidth).
const COPY_NS_PER_BYTE: f64 = 0.1;
/// A computation must beat a lookup by this factor to be stored: the margin
/// covers the memory the entry occupies and the chance it is never asked again.
const WORTH_FACTOR: f64 = 4.0;

/// Whether recomputing costs enough more than looking up to keep the result.
pub(crate) fn worth_storing(cost_ns: u64, result_bytes: usize) -> bool {
    let lookup = LOOKUP_BASE_NS as f64 + result_bytes as f64 * COPY_NS_PER_BYTE;
    cost_ns as f64 >= WORTH_FACTOR * lookup
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    callee: String,
    entry: String,
    argc: u32,
    payload: [u8; 32],
}

struct Entry {
    bytes: Arc<[u8]>,
    cost_ns: u64,
    hits: u64,
    priority: f64,
    seq: u64,
}

/// What one callee's results have done for the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalleeStats {
    /// Calls that ran (cache misses that were computed).
    pub computed: u64,
    /// Nanoseconds spent computing them.
    pub compute_ns: u64,
    /// Calls answered from the cache.
    pub hits: u64,
    /// Nanoseconds those hits did not have to compute.
    pub saved_ns: u64,
    /// Results stored, and results judged not worth storing.
    pub stored: u64,
    pub skipped_cheap: u64,
    /// Bytes of this callee's results currently held.
    pub bytes: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<Key, Entry>,
    /// Eviction order: lowest priority first, oldest first among equals.
    order: BTreeMap<(u64, u64), Key>,
    /// GDSF's inflation value: the priority of the last entry evicted.
    clock: f64,
    bytes: usize,
    seq: u64,
    callees: HashMap<String, CalleeStats>,
}

pub(crate) struct ResultCache {
    inner: Mutex<Inner>,
    max_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    evictions: AtomicU64,
    skipped_cheap: AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub entries: usize,
    pub bytes: usize,
    pub evictions: u64,
    pub skipped_cheap: u64,
    /// Nanoseconds of computation all hits avoided.
    pub saved_ns: u64,
}

impl Default for ResultCache {
    fn default() -> Self {
        Self::with_capacity(MAX_BYTES)
    }
}

fn key(callee: &str, entry: &str, argc: u32, payload: &[u8]) -> Key {
    Key {
        callee: callee.into(),
        entry: entry.into(),
        argc,
        payload: *blake3::hash(payload).as_bytes(),
    }
}

/// Positive finite priorities order the same as their bit patterns.
fn order_key(priority: f64, seq: u64) -> (u64, u64) {
    (priority.to_bits(), seq)
}

fn priority(clock: f64, hits: u64, cost_ns: u64, size: usize) -> f64 {
    clock + hits as f64 * cost_ns as f64 / size.max(1) as f64
}

impl ResultCache {
    pub(crate) fn with_capacity(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            skipped_cheap: AtomicU64::new(0),
        }
    }

    pub(crate) fn get(&self, callee: &str, entry: &str, argc: u32, payload: &[u8]) -> Option<Vec<u8>> {
        let key = key(callee, entry, argc, payload);
        let mut inner = self.inner.lock().expect("result cache poisoned");
        let clock = inner.clock;
        let Some(found) = inner.map.get_mut(&key) else {
            drop(inner);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let bytes = found.bytes.clone();
        let cost_ns = found.cost_ns;
        let (old_priority, old_seq) = (found.priority, found.seq);
        found.hits += 1;
        found.priority = priority(clock, found.hits, cost_ns, bytes.len());
        let new_priority = found.priority;
        inner.order.remove(&order_key(old_priority, old_seq));
        inner.order.insert(order_key(new_priority, old_seq), key.clone());
        let stats = inner.callees.entry(key.callee.clone()).or_default();
        stats.hits += 1;
        stats.saved_ns += cost_ns;
        drop(inner);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(bytes.to_vec())
    }

    /// Remember `result`, which took `cost_ns` to compute, unless it is cheaper to
    /// recompute than to look up or too big to keep.
    pub(crate) fn put(
        &self,
        callee: &str,
        entry: &str,
        argc: u32,
        payload: &[u8],
        result: &[u8],
        cost_ns: u64,
    ) {
        let mut inner = self.inner.lock().expect("result cache poisoned");
        let stats = inner.callees.entry(callee.to_owned()).or_default();
        stats.computed += 1;
        stats.compute_ns += cost_ns;
        if result.len() > MAX_RESULT_BYTES || result.len() > self.max_bytes {
            return;
        }
        if !worth_storing(cost_ns, result.len()) {
            stats.skipped_cheap += 1;
            drop(inner);
            self.skipped_cheap.fetch_add(1, Ordering::Relaxed);
            return;
        }
        stats.stored += 1;
        let key = key(callee, entry, argc, payload);
        if let Some(old) = inner.map.remove(&key) {
            inner.order.remove(&order_key(old.priority, old.seq));
            inner.bytes -= old.bytes.len();
            if let Some(stats) = inner.callees.get_mut(callee) {
                stats.bytes = stats.bytes.saturating_sub(old.bytes.len() as u64);
            }
        }
        // Make room: evict the lowest priorities, raising the clock to each.
        let mut evicted = 0;
        while inner.bytes + result.len() > self.max_bytes {
            let Some((&first, _)) = inner.order.iter().next() else {
                break;
            };
            let victim_key = inner.order.remove(&first).expect("first key present");
            let victim = inner.map.remove(&victim_key).expect("ordered entry present");
            inner.clock = victim.priority.max(inner.clock);
            inner.bytes -= victim.bytes.len();
            if let Some(stats) = inner.callees.get_mut(&victim_key.callee) {
                stats.bytes = stats.bytes.saturating_sub(victim.bytes.len() as u64);
            }
            evicted += 1;
        }
        inner.seq += 1;
        let seq = inner.seq;
        let value = Entry {
            bytes: Arc::from(result),
            cost_ns,
            hits: 1,
            priority: priority(inner.clock, 1, cost_ns, result.len()),
            seq,
        };
        inner.order.insert(order_key(value.priority, seq), key.clone());
        inner.bytes += result.len();
        if let Some(stats) = inner.callees.get_mut(callee) {
            stats.bytes += result.len() as u64;
        }
        inner.map.insert(key, value);
        drop(inner);
        self.stores.fetch_add(1, Ordering::Relaxed);
        self.evictions.fetch_add(evicted, Ordering::Relaxed);
    }

    /// Forget every result of `callee`, or all of them. Editing a definition
    /// already changes its hash; this is for a result that must not be reused
    /// though its inputs are unchanged.
    pub(crate) fn clear(&self, callee: Option<&str>) -> usize {
        let mut inner = self.inner.lock().expect("result cache poisoned");
        let doomed: Vec<Key> = inner
            .map
            .keys()
            .filter(|key| callee.is_none_or(|callee| key.callee == callee))
            .cloned()
            .collect();
        for key in &doomed {
            if let Some(old) = inner.map.remove(key) {
                inner.order.remove(&order_key(old.priority, old.seq));
                inner.bytes -= old.bytes.len();
            }
        }
        for (name, stats) in inner.callees.iter_mut() {
            if callee.is_none_or(|callee| name == callee) {
                stats.bytes = 0;
            }
        }
        doomed.len()
    }

    pub(crate) fn stats(&self) -> ResultCacheStats {
        let inner = self.inner.lock().expect("result cache poisoned");
        ResultCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stores: self.stores.load(Ordering::Relaxed),
            entries: inner.map.len(),
            bytes: inner.bytes,
            evictions: self.evictions.load(Ordering::Relaxed),
            skipped_cheap: self.skipped_cheap.load(Ordering::Relaxed),
            saved_ns: inner.callees.values().map(|stats| stats.saved_ns).sum(),
        }
    }

    /// Per-callee accounting, the callees that saved the most time first.
    pub(crate) fn by_callee(&self) -> Vec<(String, CalleeStats)> {
        let inner = self.inner.lock().expect("result cache poisoned");
        let mut all: Vec<_> = inner
            .callees
            .iter()
            .map(|(name, stats)| (name.clone(), stats.clone()))
            .collect();
        all.sort_by(|a, b| b.1.saved_ns.cmp(&a.1.saved_ns).then_with(|| a.0.cmp(&b.0)));
        all
    }
}

#[cfg(test)]
mod tests;

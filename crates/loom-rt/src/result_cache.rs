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
//! The cache is in memory and bounded in bytes. When the store is a file, results
//! of up to 1 MiB are also written behind to `cache.db` beside it and read back at
//! the next start ([`persist`]); without a file, or when that file cannot be used,
//! a restart empties the cache, which is always safe.
//!
//! **A result belongs to the build that computed it.** The key's inputs are the
//! call's, but the answer also depends on the host that ran it (kernels, guest ABI,
//! wasmtime). So the key digest also covers the host identity
//! ([`crate::host_identity`]: crate and wasmtime versions, executable path, length
//! and mtime), and every persisted row is tagged with it. A rebuilt binary has a
//! new identity: it never hits a previous build's rows (other digests), and its
//! start-up load deletes them.
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Bytes of results kept.
const MAX_BYTES: usize = 128 * 1024 * 1024;
/// A result larger than this is not worth the memory whatever it cost.
const MAX_RESULT_BYTES: usize = 8 << 20;

/// Callees whose statistics are kept; past this the one that saved least is
/// forgotten to make room, so the map cannot grow with every callee ever seen.
const MAX_CALLEES: usize = 4096;
/// Callees [`ResultCache::by_callee`] reports.
const TOP_CALLEES: usize = 10;

/// A lookup costs about this much before the copy: a payload hash, a lock and a
/// map probe (measured at 1 to 3 microseconds).
const LOOKUP_BASE_NS: u64 = 3_000;
/// Copying a result out costs about this per byte (memory bandwidth).
const COPY_NS_PER_BYTE: f64 = 0.1;
/// A computation must beat a lookup by this factor to be stored: the margin
/// covers the memory the entry occupies and the chance it is never asked again.
const WORTH_FACTOR: f64 = 4.0;

mod persist;

use crate::host_identity::host_identity;

/// Whether recomputing costs enough more than looking up to keep the result.
pub(crate) fn worth_storing(cost_ns: u64, result_bytes: usize) -> bool {
    let lookup = LOOKUP_BASE_NS as f64 + result_bytes as f64 * COPY_NS_PER_BYTE;
    cost_ns as f64 >= WORTH_FACTOR * lookup
}

/// What a result is stored under: the callee (kept for per-callee accounting and
/// clearing) and a BLAKE3 digest of everything that determines the result, which is
/// also the primary key of the persisted row.
#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    callee: String,
    digest: [u8; 32],
}

struct Entry {
    bytes: Arc<[u8]>,
    cost_ns: u64,
    hits: u64,
    priority: f64,
    seq: u64,
    /// Whether a row for this result was sent to (or read from) `cache.db`, so
    /// hits and eviction are reported to it.
    persisted: bool,
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
    /// Nanoseconds all hits avoided, kept apart from `callees` so forgetting a
    /// callee's statistics does not shrink it.
    saved_ns: u64,
    callees: HashMap<String, CalleeStats>,
}

impl Inner {
    /// The statistics of `name`, created if absent; at [`MAX_CALLEES`] the callee
    /// that saved least (the smallest name among equals) is forgotten first.
    fn callee_stats(&mut self, name: &str) -> &mut CalleeStats {
        if !self.callees.contains_key(name) {
            if self.callees.len() >= MAX_CALLEES {
                let weakest = self
                    .callees
                    .iter()
                    .min_by(|a, b| a.1.saved_ns.cmp(&b.1.saved_ns).then_with(|| a.0.cmp(b.0)))
                    .map(|(name, _)| name.clone());
                if let Some(weakest) = weakest {
                    self.callees.remove(&weakest);
                }
            }
            self.callees.insert(name.to_owned(), CalleeStats::default());
        }
        self.callees.get_mut(name).expect("inserted above")
    }
}

pub(crate) struct ResultCache {
    inner: Mutex<Inner>,
    max_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    evictions: AtomicU64,
    skipped_cheap: AtomicU64,
    persist: Option<persist::Persistence>,
    /// The host identity mixed into every key; see the module comment.
    identity: [u8; 32],
    /// Results read back from `cache.db` when this cache was opened.
    loaded_at_start: usize,
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
    /// Rows in `cache.db` as of its writer's last transaction; 0 without persistence.
    pub persisted_entries: usize,
    /// Results read back from `cache.db` when the cache was opened.
    pub loaded_at_start: usize,
}

impl Default for ResultCache {
    fn default() -> Self {
        Self::with_capacity(MAX_BYTES)
    }
}

/// The digest covers the host identity, the kernel fingerprint, the arity, the callee and the entry
/// (length-prefixed, so no two field splits collide) and then the payload.
fn key(
    identity: &[u8; 32],
    callee: &str,
    entry: &str,
    argc: u32,
    payload: &[u8],
    kernels: &[u8; 32],
) -> Key {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom result cache key v2");
    hasher.update(identity);
    hasher.update(kernels);
    hasher.update(&argc.to_le_bytes());
    for part in [callee, entry] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.update(payload);
    Key {
        callee: callee.into(),
        digest: *hasher.finalize().as_bytes(),
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
            persist: None,
            identity: *host_identity(),
            loaded_at_start: 0,
        }
    }

    /// The cache a runtime uses: persistent in `dir` when the store has one, in
    /// memory only otherwise.
    pub(crate) fn open(dir: Option<PathBuf>) -> Self {
        match dir {
            Some(dir) => Self::persistent(MAX_BYTES, &dir),
            None => Self::default(),
        }
    }

    /// A cache of `max_bytes` that keeps `cache.db` in `dir` and starts from what it
    /// holds. A file that cannot be used is replaced (or, failing that, ignored)
    /// with a warning; this never fails.
    pub(crate) fn persistent(max_bytes: usize, dir: &Path) -> Self {
        Self::persistent_as(max_bytes, dir, *host_identity())
    }

    /// [`Self::persistent`] for a given host identity, so a test can be another build.
    fn persistent_as(max_bytes: usize, dir: &Path, identity: [u8; 32]) -> Self {
        let mut cache = Self::with_capacity(max_bytes);
        cache.identity = identity;
        if let Some((persistence, loaded)) = persist::open(dir, max_bytes, &identity) {
            let inner = cache.inner.get_mut().expect("result cache poisoned");
            for row in loaded {
                let size = row.value.len();
                inner.seq += 1;
                let entry = Entry {
                    bytes: Arc::from(row.value),
                    cost_ns: row.cost_ns,
                    hits: row.hits,
                    priority: priority(inner.clock, row.hits, row.cost_ns, size),
                    seq: inner.seq,
                    persisted: true,
                };
                let key = Key {
                    callee: row.callee,
                    digest: row.digest,
                };
                inner
                    .order
                    .insert(order_key(entry.priority, entry.seq), key.clone());
                inner.bytes += size;
                inner.callee_stats(&key.callee).bytes += size as u64;
                inner.map.insert(key, entry);
                cache.loaded_at_start += 1;
            }
            cache.persist = Some(persistence);
        }
        cache
    }

    /// Wait until everything stored, evicted or cleared so far is in `cache.db`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn flush_persisted(&self) {
        if let Some(persistence) = &self.persist {
            persistence.flush();
        }
    }

    pub(crate) fn get(
        &self,
        callee: &str,
        entry: &str,
        argc: u32,
        payload: &[u8],
        kernels: &[u8; 32],
    ) -> Option<Vec<u8>> {
        let key = key(&self.identity, callee, entry, argc, payload, kernels);
        let mut inner = self.inner.lock().expect("result cache poisoned");
        let clock = inner.clock;
        let Some(found) = inner.map.get_mut(&key) else {
            drop(inner);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let bytes = found.bytes.clone();
        let persisted = found.persisted;
        let cost_ns = found.cost_ns;
        let (old_priority, old_seq) = (found.priority, found.seq);
        found.hits += 1;
        found.priority = priority(clock, found.hits, cost_ns, bytes.len());
        let new_priority = found.priority;
        inner.order.remove(&order_key(old_priority, old_seq));
        inner
            .order
            .insert(order_key(new_priority, old_seq), key.clone());
        let stats = inner.callee_stats(&key.callee);
        stats.hits += 1;
        stats.saved_ns += cost_ns;
        inner.saved_ns += cost_ns;
        drop(inner);
        if persisted && let Some(persistence) = &self.persist {
            persistence.hit(key.digest);
        }
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
        kernels: &[u8; 32],
        result: &[u8],
        cost_ns: u64,
    ) {
        let mut inner = self.inner.lock().expect("result cache poisoned");
        let stats = inner.callee_stats(callee);
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
        let key = key(&self.identity, callee, entry, argc, payload, kernels);
        // Values over the persistence limit stay in memory only.
        let persist_new = result.len() <= persist::MAX_VALUE_BYTES;
        if let Some(old) = inner.map.remove(&key) {
            inner.order.remove(&order_key(old.priority, old.seq));
            inner.bytes -= old.bytes.len();
            if old.persisted
                && !persist_new
                && let Some(persistence) = &self.persist
            {
                persistence.delete(key.digest);
            }
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
            let victim = inner
                .map
                .remove(&victim_key)
                .expect("ordered entry present");
            inner.clock = victim.priority.max(inner.clock);
            inner.bytes -= victim.bytes.len();
            if victim.persisted
                && let Some(persistence) = &self.persist
            {
                persistence.delete(victim_key.digest);
            }
            if let Some(stats) = inner.callees.get_mut(&victim_key.callee) {
                stats.bytes = stats.bytes.saturating_sub(victim.bytes.len() as u64);
            }
            evicted += 1;
        }
        inner.seq += 1;
        let seq = inner.seq;
        let mut value = Entry {
            bytes: Arc::from(result),
            cost_ns,
            hits: 1,
            priority: priority(inner.clock, 1, cost_ns, result.len()),
            seq,
            persisted: false,
        };
        if persist_new && let Some(persistence) = &self.persist {
            persistence.put(persist::Row {
                digest: key.digest,
                callee: callee.to_owned(),
                entry: entry.to_owned(),
                argc,
                cost_ns,
                hits: value.hits,
                value: value.bytes.clone(),
            });
            value.persisted = true;
        }
        inner
            .order
            .insert(order_key(value.priority, seq), key.clone());
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
        let dropped = self.clear_memory(callee);
        // Sent after `inner` is released: the send may wait for room, and a stuck
        // writer must not stall every `get` and `put`. A store that slipped in
        // between is a new result whose row this clear deletes, which only costs a
        // future miss; every row that existed before the clear is behind it in the
        // queue. It waits for room because a cleared result must not return after a
        // restart.
        if let Some(persistence) = &self.persist {
            persistence.clear(callee);
        }
        dropped
    }

    fn clear_memory(&self, callee: Option<&str>) -> usize {
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
            saved_ns: inner.saved_ns,
            persisted_entries: self.persist.as_ref().map_or(0, |p| p.entries() as usize),
            loaded_at_start: self.loaded_at_start,
        }
    }

    /// Per-callee accounting of the [`TOP_CALLEES`] callees that saved the most
    /// time, the most first.
    pub(crate) fn by_callee(&self) -> Vec<(String, CalleeStats)> {
        let inner = self.inner.lock().expect("result cache poisoned");
        let mut all: Vec<(&String, &CalleeStats)> = inner.callees.iter().collect();
        let order = |a: &(&String, &CalleeStats), b: &(&String, &CalleeStats)| {
            b.1.saved_ns.cmp(&a.1.saved_ns).then_with(|| a.0.cmp(b.0))
        };
        // Partition the top out in linear time, then sort only those.
        if all.len() > TOP_CALLEES {
            all.select_nth_unstable_by(TOP_CALLEES, order);
            all.truncate(TOP_CALLEES);
        }
        all.sort_by(order);
        all.into_iter()
            .map(|(name, stats)| (name.clone(), stats.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests;

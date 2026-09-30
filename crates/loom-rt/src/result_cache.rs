//! Results of isolated calls, remembered by what determines them.
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
//! effect, is never stored. The cache is in memory and bounded; a restart empties
//! it, which is always safe.
use lru::LruCache;
use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Entries kept; the least recently used is dropped past this.
const ENTRIES: usize = 4096;
/// A result larger than this is not worth the memory.
const MAX_RESULT_BYTES: usize = 1 << 20;

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    callee: String,
    entry: String,
    argc: u32,
    payload: [u8; 32],
}

pub(crate) struct ResultCache {
    entries: Mutex<LruCache<Key, Arc<[u8]>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub entries: usize,
}

impl Default for ResultCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(ENTRIES).expect("nonzero entry limit"),
            )),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
        }
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

impl ResultCache {
    pub(crate) fn get(&self, callee: &str, entry: &str, argc: u32, payload: &[u8]) -> Option<Vec<u8>> {
        let found = self
            .entries
            .lock()
            .expect("result cache poisoned")
            .get(&key(callee, entry, argc, payload))
            .cloned();
        match found {
            Some(bytes) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(bytes.to_vec())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub(crate) fn put(&self, callee: &str, entry: &str, argc: u32, payload: &[u8], result: &[u8]) {
        if result.len() > MAX_RESULT_BYTES {
            return;
        }
        self.entries
            .lock()
            .expect("result cache poisoned")
            .put(key(callee, entry, argc, payload), Arc::from(result));
        self.stores.fetch_add(1, Ordering::Relaxed);
    }

    /// Forget every result of `callee`, or all of them. Editing a definition
    /// already changes its hash; this is for a result that must not be reused
    /// though its inputs are unchanged.
    pub(crate) fn clear(&self, callee: Option<&str>) -> usize {
        let mut entries = self.entries.lock().expect("result cache poisoned");
        let Some(callee) = callee else {
            let count = entries.len();
            entries.clear();
            return count;
        };
        let doomed: Vec<Key> = entries
            .iter()
            .filter(|(key, _)| key.callee == callee)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &doomed {
            entries.pop(key);
        }
        doomed.len()
    }

    pub(crate) fn stats(&self) -> ResultCacheStats {
        ResultCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stores: self.stores.load(Ordering::Relaxed),
            entries: self.entries.lock().expect("result cache poisoned").len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_is_found_by_callee_entry_arity_and_arguments_only() {
        let cache = ResultCache::default();
        assert!(cache.get("f", "main", 1, b"x").is_none());
        cache.put("f", "main", 1, b"x", b"42");
        assert_eq!(cache.get("f", "main", 1, b"x").unwrap(), b"42");
        for (callee, entry, argc, payload) in [
            ("g", "main", 1, &b"x"[..]),
            ("f", "other", 1, b"x"),
            ("f", "main", 2, b"x"),
            ("f", "main", 1, b"y"),
        ] {
            assert!(cache.get(callee, entry, argc, payload).is_none());
        }
        assert_eq!(
            cache.stats(),
            ResultCacheStats { hits: 1, misses: 5, stores: 1, entries: 1 }
        );
    }

    #[test]
    fn clearing_one_callee_leaves_the_others_and_clearing_all_empties() {
        let cache = ResultCache::default();
        cache.put("f", "main", 0, b"a", b"1");
        cache.put("f", "main", 0, b"b", b"2");
        cache.put("g", "main", 0, b"a", b"3");
        assert_eq!(cache.clear(Some("f")), 2);
        assert!(cache.get("f", "main", 0, b"a").is_none());
        assert_eq!(cache.get("g", "main", 0, b"a").unwrap(), b"3");
        assert_eq!(cache.clear(None), 1);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn an_oversized_result_is_not_kept_and_the_oldest_entry_goes_first() {
        let cache = ResultCache::default();
        cache.put("f", "main", 0, b"big", &vec![0; MAX_RESULT_BYTES + 1]);
        assert_eq!(cache.stats().entries, 0);
        for index in 0..=ENTRIES {
            cache.put("f", "main", 0, &index.to_le_bytes(), b"r");
        }
        assert_eq!(cache.stats().entries, ENTRIES);
        assert!(cache.get("f", "main", 0, &0usize.to_le_bytes()).is_none());
    }
}
